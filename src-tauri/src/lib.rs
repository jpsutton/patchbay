pub mod api;
mod attachments;
mod chat;
mod claude_auth;
mod code;
mod db;
#[cfg(feature = "desktop")]
mod daemon_link;
#[cfg(feature = "desktop")]
mod desktop;
mod files;
mod gateway;
mod handoff;
pub mod ipc;
mod mesh;
#[cfg(all(feature = "desktop", target_os = "linux"))]
mod portal_shortcuts;
mod prefs;
mod providers;
mod speech;
mod terminal;
mod usage;
pub mod agent_messaging;
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
    PermissionRule, Prompt, Provider, SearchHit, SidebarEntry, new_id,
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
    /// Local authenticated bridge used by the Patchbay-owned agent messaging MCP server.
    agent_message_bridge: Arc<std::sync::OnceLock<agent_messaging::AgentMessagingBridge>>,
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
    fn new(
        db: Arc<Db>,
        data_dir: PathBuf,
        mesh: Mesh,
        emit: mesh::node::UiSink,
    ) -> Self {
        let state = AppState {
            db,
            data_dir,
            chat: Arc::default(),
            code: Arc::default(),
            terminals: Arc::default(),
            mesh: mesh.clone(),
            agent_message_bridge: Arc::default(),
            emit: emit.clone(),
            pulling: Arc::default(),
            live: Arc::default(),
            remote_terms: Arc::default(),
            desktop: Arc::default(),
        };
        state.code.plan.set_accounts(load_plan_accounts(&state.db));
        if let Some(r) = state.db.get_setting(PLAN_RESUME_KEY).ok().flatten().and_then(|j| serde_json::from_str(&j).ok()) {
            state.code.plan.set_resumes(r);
        }
        if let Some(l) = state
            .db
            .get_setting(CURSOR_PLAN_KEY)
            .ok()
            .flatten()
            .and_then(|j| serde_json::from_str(&j).ok())
        {
            state.code.cursor_plan.record(l);
        }
        if let Some(r) = state
            .db
            .get_setting(CURSOR_PLAN_RESUME_KEY)
            .ok()
            .flatten()
            .and_then(|j| serde_json::from_str(&j).ok())
        {
            state.code.cursor_plan.set_resumes(r);
        }
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
    /// The agent's latest `plan` update (its to-do list). A window that attaches after the plan
    /// was written has no other way to get it: the saved transcript doesn't hold it, and the
    /// event ring only reaches back to the last finished turn.
    plan: Option<serde_json::Value>,
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
    /// For Claude login: another Claude Code config directory (`CLAUDE_CONFIG_DIR`) to sign in with.
    #[serde(default)]
    claude_config_dir: Option<String>,
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
        claude_config_dir: input
            .claude_config_dir
            .map(|d| d.trim().to_owned())
            .filter(|d| !d.is_empty() && auth_mode == db::AuthMode::ClaudeLogin),
        auth_mode,
    };
    if p.name.is_empty() || p.base_url.is_empty() {
        return Err(CmdError("name and base URL are required".into()));
    }
    if let Some(d) = &p.claude_config_dir {
        if !claude_auth::resolve_dir(Some(d))?.is_dir() {
            return Err(CmdError(format!("Claude config directory {d} does not exist")));
        }
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
        providers::Auth::ClaudeLogin => Some(claude_auth::access_token(p.claude_config_dir.as_deref()).await?),
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

/// One clip from a speech server. The window or browser plays the bytes; this process holds the key.
async fn synthesize_speech(
    state: &AppState,
    provider_id: String,
    base_url: String,
    model: String,
    voice: String,
    text: String,
) -> CmdResult<speech::SpeechAudio> {
    Ok(speech::synthesize(&state.db, &provider_id, &base_url, &model, &voice, &text).await?)
}

/// Part of a reply, rewritten by the user's chosen model so it reads well aloud.
async fn rewrite_for_speech(state: &AppState, model_ref: String, text: String) -> CmdResult<String> {
    Ok(speech::rewrite_for_speech(&state.db, &model_ref, &text).await?)
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
    // A lower limit applies to turns already running.
    if old.usage_limit != prefs.usage_limit {
        enforce_plan_limit(state);
    }
    if old.cursor_usage_limit != prefs.cursor_usage_limit {
        enforce_cursor_plan_limit(state);
    }
    // Other windows on this backend (browser tabs, a desktop app attached to patchbayd) show them.
    (state.emit)(PREFS_CHANGED_EVENT, serde_json::to_value(&prefs).unwrap_or_default());
    Ok(())
}

/// App event with the preferences just saved.
pub const PREFS_CHANGED_EVENT: &str = "prefs://changed";

/// Whether web search has a Brave API key, and where it comes from (`env`, `keychain`, `database`).
async fn web_search_status(state: &AppState) -> CmdResult<serde_json::Value> {
    let found = providers::web_search_key(&state.db);
    Ok(serde_json::json!({ "configured": found.is_some(), "source": found.map(|(_, s)| s) }))
}

/// What Patchbay's agent finds in the user's Claude Code setup (and `cwd`'s project), for Settings.
async fn claude_overview(state: &AppState, cwd: Option<String>) -> CmdResult<serde_json::Value> {
    let prefs = prefs::load(&state.db);
    let cwd = cwd.map(std::path::PathBuf::from);
    Ok(tokio::task::spawn_blocking(move || code::native::claude_overview(&prefs, cwd.as_deref())).await.map_err(anyhow::Error::from)?)
}

async fn set_web_search_key(state: &AppState, key: String) -> CmdResult<()> {
    Ok(providers::set_web_search_key(&state.db, &key)?)
}

async fn clear_web_search_key(state: &AppState) -> CmdResult<()> {
    Ok(providers::clear_web_search_key(&state.db)?)
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

/// An image attachment as a data URL, at the size the agent was sent.
async fn attachment_image(state: &AppState, id: String) -> CmdResult<String> {
    let att = state
        .db
        .get_attachment(&id)?
        .ok_or_else(|| CmdError("attachment not found".into()))?;
    let dir = state.data_dir.clone();
    Ok(tokio::task::spawn_blocking(move || attachments::image_data_url(&dir, &att)).await??)
}

// ---------------- code ----------------
//
// A Code session runs on one node, its host. Commands for a session hosted elsewhere in the mesh
// go to that node as RPCs (`MeshRpc` serves them there); agent events come back as mesh events.

async fn list_code_sessions(state: &AppState) -> CmdResult<Vec<CodeSession>> {
    Ok(state.db.list_code_sessions()?)
}

/// A new Code session in `cwd`, here or, with `host`, on that computer.
async fn create_code_session(
    state: &AppState,
    cwd: String,
    model_ref: Option<String>,
    agent: Option<String>,
    host: Option<String>,
) -> CmdResult<CodeSession> {
    let Some(host) = host.filter(|h| h != state.mesh.node_id()) else {
        return Ok(create_code_session_local(state, &cwd, model_ref.as_deref(), agent)?);
    };
    let p = serde_json::json!({ "host": host, "cwd": cwd, "modelRef": model_ref, "agent": agent });
    let created = gateway::on_host(state, &p, "code.create").await?;
    let id = created["id"].as_str().ok_or_else(|| CmdError("the host returned no session".into()))?;
    // The host's new row reaches this computer through mesh sync; wait for it, so the caller
    // can open the session from its list.
    for _ in 0..100 {
        if let Some(s) = state.db.get_code_session(id)? {
            return Ok(s);
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    Err(CmdError(format!("the session started on {}, but hasn't reached this computer yet", state.mesh.member_name(&host))))
}

/// Claude Code's sessions on this computer, marking those already imported.
async fn list_claude_sessions(state: &AppState) -> CmdResult<Vec<code::import_claude::ClaudeSessionInfo>> {
    let dir = code::import_claude::projects_dir()?;
    let mut found = tokio::task::spawn_blocking(move || code::import_claude::list(&dir)).await?;
    for f in &mut found {
        f.imported = state
            .db
            .get_setting(&format!("import.claude.{}", f.id))?
            .filter(|id| state.db.get_code_session(id).ok().flatten().is_some());
    }
    Ok(found)
}

/// Make a Code session from each of Claude Code's sessions `ids`, with `agent` (default: the
/// preferred one) to continue them. A session imported before opens again instead of copying.
async fn import_claude_sessions(state: &AppState, ids: Vec<String>, agent: Option<String>) -> CmdResult<Vec<CodeSession>> {
    let projects = code::import_claude::projects_dir()?;
    let found = {
        let p = projects.clone();
        tokio::task::spawn_blocking(move || code::import_claude::list(&p)).await?
    };
    let mut made = Vec::new();
    for id in ids {
        let key = format!("import.claude.{id}");
        if let Some(existing) = state.db.get_setting(&key)?.and_then(|sid| state.db.get_code_session(&sid).ok().flatten()) {
            made.push(existing);
            continue;
        }
        let path = found
            .iter()
            .find(|f| f.id == id)
            .and_then(|f| projects.read_dir().ok()?.flatten().map(|d| d.path().join(format!("{}.jsonl", f.id))).find(|p| p.is_file()))
            .ok_or_else(|| CmdError(format!("Claude Code session {id} not found")))?;
        let conv = tokio::task::spawn_blocking(move || code::import_claude::convert(&path)).await??;
        // The original folder may be gone (a deleted worktree): use the nearest one that exists.
        let mut cwd = std::path::PathBuf::from(&conv.cwd);
        while !cwd.is_dir() && cwd.pop() {}
        let cwd = if cwd.as_os_str().is_empty() || !cwd.is_dir() {
            code::agents::home_dir().ok_or_else(|| CmdError("no folder to open the session in".into()))?
        } else {
            cwd
        };
        let s = create_code_session_local(state, &cwd.to_string_lossy(), None, agent.clone())?;
        state.db.set_code_session_title(&s.id, &chat::clip(&conv.title, 80), true)?;
        state.db.set_code_session_times(&s.id, conv.created_ms.max(1), conv.updated_ms.max(conv.created_ms).max(1))?;
        state.db.set_code_transcript(&s.id, &conv.transcript)?;
        state.db.set_setting(&format!("mesh.handoff.{}", s.id), &handoff::import_prompt(Some(&conv.transcript), "Claude Code"))?;
        state.db.set_setting(&key, &s.id)?;
        made.push(state.db.get_code_session(&s.id)?.unwrap_or(s));
    }
    Ok(made)
}

/// A new Code session hosted here, in `cwd` on this computer.
fn create_code_session_local(
    state: &AppState,
    cwd: &str,
    model_ref: Option<&str>,
    agent: Option<String>,
) -> anyhow::Result<CodeSession> {
    // Native folder pickers can return a trailing slash (the KDE portal does); `/a/b` and `/a/b/`
    // are one folder, and the same permission rules and recent-folder entry must serve both.
    let cwd = gateway::folder_key(cwd);
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

/// How often the age limit for Code sessions (`Preferences::auto_archive_days`) is applied.
const AUTO_ARCHIVE_EVERY: std::time::Duration = std::time::Duration::from_secs(60 * 60);

/// Archive the Code sessions this node hosts that nobody prompted for `auto_archive_days` days,
/// except pinned ones and ones mid-turn. Returns how many it archived.
async fn auto_archive_sweep(state: &AppState) -> anyhow::Result<usize> {
    let Some(days) = prefs::load(&state.db).auto_archive_days else {
        return Ok(0);
    };
    let cutoff = db::now_ms() - i64::from(days) * 24 * 60 * 60 * 1000;
    let mut archived = 0;
    for id in state.db.stale_code_session_ids(cutoff)? {
        if state.code.is_busy(&id) || state.code.is_moving(&id) {
            continue;
        }
        match archive_code_session(state, &id, true).await {
            Ok(()) => archived += 1,
            Err(e) => tracing::warn!("auto-archive of session {id}: {e:#}"),
        }
    }
    if archived > 0 {
        (state.emit)("mesh://changed", serde_json::json!({ "tables": ["code_sessions"] }));
    }
    Ok(archived)
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
    if let CodeEvent::PlanLimits { limits } = env.event {
        // Only sessions on a Claude plan report limits; one that can't be placed counts as the default login's.
        let (account, name) = plan_account(state, &prefs::load(&state.db), &env.code_session_id).unwrap_or_else(default_plan_account);
        let tagged = serde_json::json!({ "account": account, "name": name, "limits": limits });
        state.mesh.broadcast_event(MESH_PLAN_LIMITS_EVENT, tagged);
        adopt_plan_limits(state, &account, name, limits, true);
        return;
    }
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
            state.code.plan.turn_ended(&env.code_session_id);
            state.code.cursor_plan.turn_ended(&env.code_session_id);
        }
        if matches!(env.event, CodeEvent::UserPrompt { delivery: code::acp::Delivery::Sent, .. }) {
            state.code.set_busy(&env.code_session_id, true);
            let _ = state.code.cursor_plan.cancel_resume(&env.code_session_id);
            if state.code.plan.cancel_resume(&env.code_session_id) {
                save_resumes(state);
            }
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
                CodeEvent::Config { .. } => {
                    // A window attaching later replays `ready`, so it has to carry the model, modes
                    // and options as they are now, not as the agent started. Same merge as the
                    // front end's `config` event: models and modes stay when absent.
                    if let Some(r) = live.get_mut(&id).and_then(|l| l.ready.as_mut()) {
                        for key in ["models", "modes"] {
                            if !ev[key].is_null() {
                                r[key] = ev[key].clone();
                            }
                        }
                        r["configOptions"] = ev["configOptions"].clone();
                    }
                }
                CodeEvent::AuthRequired { .. } => live.entry(id).or_default().auth = Some(ev),
                CodeEvent::AuthResolved => {
                    if let Some(l) = live.get_mut(&id) {
                        l.auth = None;
                    }
                }
                CodeEvent::Usage { .. } => live.entry(id).or_default().usage = Some(ev),
                CodeEvent::Update { update } if update["sessionUpdate"] == "plan" => {
                    live.entry(id).or_default().plan = Some(ev)
                }
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

/// Each Claude account's last reported plan usage and its distinct reports of the last eight
/// days, saved so the panel and the guard have them after a restart; reset times say when they go stale.
const PLAN_ACCOUNTS_KEY: &str = "claude.planByAccount";
/// Before accounts were told apart: the one account's limits and history, read once to migrate.
const PLAN_LIMITS_KEY: &str = "claude.planLimits";
const PLAN_HISTORY_KEY: &str = "claude.planHistory";
const PLAN_LIMITS_EVENT: &str = "code://plan-limits";
/// A report one member saw, shared with the others, tagged with its account (`claude_auth::account`).
const MESH_PLAN_LIMITS_EVENT: &str = "mesh://plan-limits";

/// Take a plan report seen here, or one from another member that is newer than ours for its account.
fn adopt_plan_limits(state: &AppState, account: &str, name: Option<String>, limits: code::plan_limits::PlanLimits, seen_here: bool) {
    if !seen_here && state.code.plan.account(account).latest.is_some_and(|old| old.observed_at >= limits.observed_at) {
        return;
    }
    state.code.plan.record(account, name, limits);
    let _ = state.db.set_setting(PLAN_ACCOUNTS_KEY, &serde_json::to_string(&state.code.plan.accounts()).unwrap_or_default());
    (state.emit)(PLAN_LIMITS_EVENT, serde_json::to_value(plan_views(state)).unwrap_or_default());
    enforce_plan_limit(state);
}

/// Sessions the usage guard stopped that get prompted again when the limit lifts
/// (`UsageLimit::auto_resume`), kept across restarts.
const PLAN_RESUME_KEY: &str = "claude.planResume";

fn save_resumes(state: &AppState) {
    let _ = state.db.set_setting(PLAN_RESUME_KEY, &serde_json::to_string(&state.code.plan.resumes()).unwrap_or_default());
}

fn load_plan_accounts(db: &Db) -> std::collections::BTreeMap<String, code::plan_limits::Account> {
    let get = |k| db.get_setting(k).ok().flatten();
    if let Some(m) = get(PLAN_ACCOUNTS_KEY).and_then(|j| serde_json::from_str(&j).ok()) {
        return m;
    }
    let old = code::plan_limits::Account {
        latest: get(PLAN_LIMITS_KEY).and_then(|j| serde_json::from_str(&j).ok()),
        history: get(PLAN_HISTORY_KEY).and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default(),
        name: None,
    };
    if old == Default::default() {
        return Default::default();
    }
    let (account, name) = default_plan_account();
    [(account, code::plan_limits::Account { name, ..old })].into()
}

/// The account Claude Code itself signs in with, and its organization's name.
fn default_plan_account() -> (String, Option<String>) {
    claude_auth::resolve_dir(None).map(|d| claude_auth::account(&d)).unwrap_or_default()
}

/// One Claude account's usage, named for the panel.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PlanView {
    account: String,
    /// "Claude Code login" for the default account, else the Claude login providers that use it,
    /// else its organization's name.
    label: String,
    #[serde(flatten)]
    usage: code::plan_limits::Account,
}

fn plan_views(state: &AppState) -> Vec<PlanView> {
    let (default, _) = default_plan_account();
    let providers = state.db.list_providers().unwrap_or_default();
    state
        .code
        .plan
        .accounts()
        .into_iter()
        .map(|(account, usage)| {
            let names: Vec<_> = providers
                .iter()
                .filter(|(p, _)| p.auth_mode == db::AuthMode::ClaudeLogin && provider_plan_account(p).as_deref() == Some(account.as_str()))
                .map(|(p, _)| p.name.as_str())
                .collect();
            let label = if account == default {
                "Claude Code login".to_owned()
            } else if names.is_empty() {
                usage.name.clone().unwrap_or_else(|| account.clone())
            } else {
                names.join(", ")
            };
            PlanView { account, label, usage }
        })
        .collect()
}

async fn plan_accounts(state: &AppState) -> CmdResult<Vec<PlanView>> {
    Ok(plan_views(state))
}

/// Last Cursor plan report saved for the panel and the guard; reset times say when it goes stale.
const CURSOR_PLAN_KEY: &str = "cursor.planLimits";
const CURSOR_PLAN_EVENT: &str = "code://cursor-plan-limits";
/// A report one member fetched, shared with the others, tagged with its Cursor account id.
const MESH_CURSOR_PLAN_EVENT: &str = "mesh://cursor-plan-limits";
const CURSOR_PLAN_RESUME_KEY: &str = "cursor.planResume";

fn save_cursor_plan(state: &AppState) {
    if let Some(l) = state.code.cursor_plan.latest() {
        let _ = state.db.set_setting(CURSOR_PLAN_KEY, &serde_json::to_string(&l).unwrap_or_default());
    }
}

fn save_cursor_resumes(state: &AppState) {
    let _ = state
        .db
        .set_setting(CURSOR_PLAN_RESUME_KEY, &serde_json::to_string(&state.code.cursor_plan.resumes()).unwrap_or_default());
}

/// Take a Cursor plan report seen here, or a newer one from another member for the same account.
fn adopt_cursor_plan(state: &AppState, mut limits: code::cursor_plan::CursorPlanLimits, seen_here: bool) {
    if seen_here {
        state.code.cursor_plan.record(limits.clone());
    } else {
        limits.observed_at = limits.observed_at.min(code::cursor_plan::now());
        let local = state.code.cursor_plan.latest().map(|l| l.account_id);
        // Peers with no local Cursor login still show a report; those with a different account ignore it.
        if local.as_deref().is_some_and(|a| a != limits.account_id) {
            return;
        }
        if !state.code.cursor_plan.adopt_mesh(limits.clone(), local.as_deref()) {
            return;
        }
    }
    save_cursor_plan(state);
    (state.emit)(CURSOR_PLAN_EVENT, serde_json::to_value(&limits).unwrap_or_default());
    if seen_here {
        let tagged = serde_json::json!({ "accountId": limits.account_id, "limits": limits });
        state.mesh.broadcast_event(MESH_CURSOR_PLAN_EVENT, tagged);
    }
    enforce_cursor_plan_limit(state);
}

async fn cursor_plan_limits(state: &AppState) -> CmdResult<Option<code::cursor_plan::CursorPlanLimits>> {
    Ok(state.code.cursor_plan.latest())
}

/// The Cursor plan blade is open: poll while it is, even with no running Cursor session.
async fn cursor_plan_watching(state: &AppState, open: bool) -> CmdResult<()> {
    state.code.cursor_plan.set_blade_open(open);
    if open {
        poll_cursor_plan(state).await;
    }
    Ok(())
}

fn on_cursor_session(state: &AppState, id: &str) -> bool {
    state
        .db
        .get_code_session(id)
        .ok()
        .flatten()
        .is_some_and(|s| code::agents::AgentKind::parse(&s.agent) == Some(code::agents::AgentKind::Cursor))
}

fn any_running_cursor(state: &AppState) -> bool {
    state
        .db
        .list_code_sessions()
        .ok()
        .into_iter()
        .flatten()
        .any(|s| s.agent == code::agents::AgentKind::Cursor.id() && state.code.is_running(&s.id))
}

fn enforce_cursor_plan_limit(state: &AppState) {
    let prefs = prefs::load(&state.db);
    let now = code::cursor_plan::now();
    let Some(hit) = state.code.cursor_plan.hit(&prefs.cursor_usage_limit, now) else {
        return;
    };
    for id in state.code.busy_sessions() {
        if !on_cursor_session(state, &id) || !state.code.cursor_plan.start_stopping(&id) {
            continue;
        }
        tracing::info!("cursor usage limit: stopping {id}");
        if state.code.send(&id, AgentCmd::Cancel).is_err() {
            state.code.cursor_plan.turn_ended(&id);
            continue;
        }
        let mut message = format!("Stopped. {}", hit.message(now));
        if prefs.cursor_usage_limit.auto_resume {
            state.code.cursor_plan.schedule_resume(&id, hit.resets_at);
            save_cursor_resumes(state);
            message.push_str(" Patchbay will tell the agent to continue once the cycle resets.");
        }
        on_code_event(
            state,
            code::acp::Envelope { code_session_id: id, event: code::acp::CodeEvent::Notice { message } },
        );
    }
}

const CURSOR_RESUME_PROMPT: &str =
    "Patchbay stopped your last turn at the Cursor plan's usage limit. The cycle has reset, so continue where you left off.";

async fn resume_after_cursor_limit(state: &AppState) {
    let prefs = prefs::load(&state.db);
    if !prefs.cursor_usage_limit.enabled || !prefs.cursor_usage_limit.auto_resume {
        if state.code.cursor_plan.clear_resumes() {
            save_cursor_resumes(state);
        }
        return;
    }
    let now = code::cursor_plan::now();
    let mut due = state.code.cursor_plan.take_due(now - RESUME_GRACE_SECS);
    if due.is_empty() {
        return;
    }
    due.retain(|id| {
        let hit = state.code.cursor_plan.hit(&prefs.cursor_usage_limit, now);
        hit.inspect(|h| state.code.cursor_plan.schedule_resume(id, h.resets_at)).is_none()
    });
    save_cursor_resumes(state);
    for id in due {
        if !on_cursor_session(state, &id) {
            continue;
        }
        if let Err(e) = resume_cursor_session(state, &id).await {
            tracing::warn!("cursor usage limit: resuming {id}: {e:#}");
        }
    }
}

async fn resume_cursor_session(state: &AppState, id: &str) -> anyhow::Result<()> {
    let Some(session) = state.db.get_code_session(id)? else {
        return Ok(());
    };
    if session.archived_at.is_some() || state.code.is_busy(id) || state.code.is_moving(id) {
        return Ok(());
    }
    if !state.code.is_running(id) {
        code_start_local(state, id.to_owned()).await?;
    }
    let _ = code_prompt_local(
        state,
        id.to_owned(),
        CURSOR_RESUME_PROMPT.into(),
        Vec::new(),
        Vec::new(),
        "system".into(),
        None,
        false,
    )
    .await?;
    Ok(())
}

/// Poll Cursor's dashboard when a Cursor session is running here or the blade is open, unless a
/// fresh report (local or mesh) for this account already covers the window.
async fn poll_cursor_plan(state: &AppState) {
    if !state.code.cursor_plan.blade_open() && !any_running_cursor(state) {
        return;
    }
    let now = code::cursor_plan::now();
    if state.code.cursor_plan.latest().is_some_and(|l| l.is_fresh(now)) {
        return;
    }
    let prefs = prefs::load(&state.db);
    let bin = code::cursor_plan::cursor_agent_bin(&prefs.agent_paths);
    let Some(auth) = code::cursor_plan::load_auth(bin.as_deref()) else {
        tracing::debug!("cursor plan: no local Cursor login");
        return;
    };
    // A mesh peer may have filled in the same account while we resolved auth.
    if state
        .code
        .cursor_plan
        .latest()
        .is_some_and(|l| l.account_id == auth.account_id && l.is_fresh(now))
    {
        return;
    }
    match code::cursor_plan::fetch(&auth).await {
        Ok(limits) => adopt_cursor_plan(state, limits, true),
        Err(e) => tracing::debug!("cursor plan: {e:#}"),
    }
}

fn provider_plan_account(p: &Provider) -> Option<String> {
    claude_auth::resolve_dir(p.claude_config_dir.as_deref()).ok().map(|d| claude_auth::account(&d).0)
}

// The ACP wire log (Settings > Diagnostics): this computer's agents only, kept in memory.

async fn wire_log_status(_state: &AppState) -> CmdResult<code::wire::Status> {
    Ok(code::wire::status())
}

async fn wire_log_set(_state: &AppState, on: bool) -> CmdResult<code::wire::Status> {
    Ok(code::wire::set(on))
}

/// The recorded frames, oldest first; with `last`, only the newest that many.
async fn wire_log_frames(
    _state: &AppState,
    session: Option<String>,
    last: Option<usize>,
) -> CmdResult<Vec<std::sync::Arc<code::wire::Frame>>> {
    Ok(code::wire::frames(session.as_deref(), last))
}

async fn wire_log_clear(_state: &AppState) -> CmdResult<code::wire::Status> {
    code::wire::clear();
    Ok(code::wire::status())
}

/// Write the log to a file under the data folder; returns its path.
async fn wire_log_save(state: &AppState) -> CmdResult<String> {
    let path = code::wire::save(&state.data_dir.join("diagnostics"))?;
    Ok(path.to_string_lossy().into_owned())
}

/// The Claude plan account the session runs on, if it runs on one with usage limits: Claude
/// Code on its own sign-in (not on a Patchbay provider, which bills by the token), or Patchbay's
/// agent on a model whose provider uses a Claude login. Accounts are `claude_auth::account`s.
fn plan_account(state: &AppState, prefs: &prefs::Preferences, id: &str) -> Option<(String, Option<String>)> {
    use code::agents::AgentKind;
    let session = state.db.get_code_session(id).ok().flatten()?;
    match AgentKind::parse(&session.agent) {
        Some(AgentKind::ClaudeCode) if !prefs.agent_providers.contains_key(AgentKind::ClaudeCode.id()) => {
            Some(default_plan_account())
        }
        Some(AgentKind::Patchbay) => session
            .model_ref
            .and_then(|r| state.db.get_model(&r).ok().flatten())
            .and_then(|m| state.db.get_provider(&m.provider_id).ok().flatten())
            .filter(|(p, _)| p.auth_mode == db::AuthMode::ClaudeLogin)
            .and_then(|(p, _)| claude_auth::resolve_dir(p.claude_config_dir.as_deref()).ok())
            .map(|d| claude_auth::account(&d)),
        _ => None,
    }
}

/// Cancel every running turn on a Claude plan account past its usage limit. The agent stops
/// cleanly, its session stays resumable, and queued prompts go back to their composers.
fn enforce_plan_limit(state: &AppState) {
    let prefs = prefs::load(&state.db);
    let now = code::plan_limits::now();
    for id in state.code.busy_sessions() {
        let Some(hit) = plan_account(state, &prefs, &id).and_then(|(a, _)| state.code.plan.hit(&a, &prefs.usage_limit, now)) else {
            continue;
        };
        if !state.code.plan.start_stopping(&id) {
            continue;
        }
        tracing::info!("usage limit: stopping {id} ({} at {}%)", hit.what, hit.percent);
        if state.code.send(&id, AgentCmd::Cancel).is_err() {
            state.code.plan.turn_ended(&id);
            continue;
        }
        let mut message = format!("Stopped. {}", hit.message(prefs.usage_limit.percent, now));
        if prefs.usage_limit.auto_resume {
            state.code.plan.schedule_resume(&id, hit.resets_at);
            save_resumes(state);
            message.push_str(" Patchbay will tell the agent to continue once the limit resets.");
        }
        on_code_event(
            state,
            code::acp::Envelope { code_session_id: id, event: code::acp::CodeEvent::Notice { message } },
        );
    }
}

/// How often sessions waiting on the usage limit are checked for resuming.
const RESUME_EVERY: std::time::Duration = std::time::Duration::from_secs(30);
/// Waits this long past a window's reset before prompting, so the plan has really lifted.
const RESUME_GRACE_SECS: i64 = 60;
const RESUME_PROMPT: &str = "Patchbay stopped your last turn at the Claude plan's usage limit. The limit has reset, so continue where you left off.";

/// Prompt the sessions the usage guard stopped, once their limit has reset and no other window
/// holds. Does nothing, and forgets them, while `UsageLimit::auto_resume` is off.
async fn resume_after_limit(state: &AppState) {
    let prefs = prefs::load(&state.db);
    if !prefs.usage_limit.enabled || !prefs.usage_limit.auto_resume {
        if state.code.plan.clear_resumes() {
            save_resumes(state);
        }
        return;
    }
    let now = code::plan_limits::now();
    let mut due = state.code.plan.take_due(now - RESUME_GRACE_SECS);
    if due.is_empty() {
        return;
    }
    // A session waits on its own account: another of its windows may still hold.
    due.retain(|id| {
        let hit = plan_account(state, &prefs, id).and_then(|(a, _)| state.code.plan.hit(&a, &prefs.usage_limit, now));
        hit.inspect(|h| state.code.plan.schedule_resume(id, h.resets_at)).is_none()
    });
    save_resumes(state);
    for id in due {
        if let Err(e) = resume_session(state, &id).await {
            tracing::warn!("usage limit: resuming {id}: {e:#}");
        }
    }
}

async fn resume_session(state: &AppState, id: &str) -> anyhow::Result<()> {
    let Some(session) = state.db.get_code_session(id)? else {
        return Ok(());
    };
    if session.archived_at.is_some() || state.code.is_busy(id) || state.code.is_moving(id) {
        return Ok(());
    }
    if !state.code.is_running(id) {
        code_start_local(state, id.to_owned()).await?;
    }
    tracing::info!("usage limit: resuming {id}");
    let me = state.mesh.node_id().to_owned();
    code_prompt_local(state, id.to_owned(), RESUME_PROMPT.to_owned(), Vec::new(), Vec::new(), me, None, false).await?;
    Ok(())
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
    let message_server = state
        .agent_message_bridge
        .get()
        .map(|bridge| bridge.server_definition(&id))
        .transpose()?;
    // `start` may block briefly while a previous agent for this session shuts down.
    tokio::task::spawn_blocking(move || code.start(sink, db, &dir, &id, message_server)).await??;
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
/// events that set up its controls (session ready, open permission requests, usage) and the
/// agent's plan.
fn code_attach_local(state: &AppState, id: &str) -> serde_json::Value {
    let running = state.code.is_running(id);
    let live = state.live.lock().unwrap();
    let mut events = Vec::new();
    if running && let Some(l) = live.get(id) {
        events.extend(l.ready.clone());
        events.extend(l.auth.clone());
        events.extend(l.permissions.iter().map(|(_, e)| e.clone()));
        events.extend(l.usage.clone());
        events.extend(l.plan.clone());
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
    prompt_id: Option<String>,
    force: bool,
) -> anyhow::Result<code::acp::Delivery> {
    ensure_local_host(state, &id)?;
    let prefs = prefs::load(&state.db);
    if let Some(hit) =
        plan_account(state, &prefs, &id).and_then(|(a, _)| state.code.plan.hit(&a, &prefs.usage_limit, code::plan_limits::now()))
    {
        anyhow::bail!("Not sent. {}", hit.message(prefs.usage_limit.percent, code::plan_limits::now()));
    }
    if on_cursor_session(state, &id)
        && let Some(hit) = state.code.cursor_plan.hit(&prefs.cursor_usage_limit, code::cursor_plan::now())
    {
        anyhow::bail!("Not sent. {}", hit.message(code::cursor_plan::now()));
    }
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
    // The agent thread tells every viewer about the prompt, and whether it went out now, mid-turn
    // or after the running turn.
    let msg = code::acp::UserMessage {
        id: prompt_id.filter(|p| !p.is_empty()).unwrap_or_else(db::new_id),
        blocks,
        text,
        origin,
        attachments: serde_json::to_value(&atts)?,
    };
    let delivery = state.code.prompt(&id, msg, force).await?;
    if note.is_some() {
        state.db.with_conn(|c| c.execute("DELETE FROM settings WHERE key = ?1", [&note_key]))?;
    }
    Ok(delivery)
}

/// Send an agent-originated message to the named Code session, routing to its host over the mesh.
pub(crate) fn list_agent_sessions(state: &AppState, source_session_id: &str) -> anyhow::Result<serde_json::Value> {
    let source = state
        .db
        .get_code_session(source_session_id)?
        .ok_or_else(|| anyhow::anyhow!("source Code session was not found"))?;
    if source.host_node.as_deref() != Some(state.mesh.node_id()) || !state.code.is_running(source_session_id) {
        anyhow::bail!("the sending agent session is no longer running on this mesh member");
    }
    let sessions: Vec<serde_json::Value> = state
        .db
        .list_code_sessions()?
        .into_iter()
        .filter(|session| session.archived_at.is_none())
        .map(|session| {
            let host = session.host_node.unwrap_or_default();
            serde_json::json!({
                "sessionId": session.id,
                "title": session.title,
                "agent": session.agent,
                "hostNode": host,
                "hostName": state.mesh.member_name(&host),
                "online": host == state.mesh.node_id() || state.mesh.connected(&host),
            })
        })
        .collect();
    Ok(serde_json::json!(sessions))
}

pub(crate) async fn send_agent_message(
    state: &AppState,
    from_node: &str,
    source_session_id: &str,
    target_session_id: &str,
    message: &str,
) -> anyhow::Result<serde_json::Value> {
    let message = message.trim();
    if message.is_empty() {
        anyhow::bail!("message cannot be empty");
    }
    if message.len() > 16 * 1024 {
        anyhow::bail!("message is longer than 16 KiB");
    }
    if source_session_id == target_session_id {
        anyhow::bail!("a session cannot message itself");
    }
    let source = state
        .db
        .get_code_session(source_session_id)?
        .ok_or_else(|| anyhow::anyhow!("source Code session was not found"))?;
    if source.host_node.as_deref() != Some(from_node) || source.archived_at.is_some() {
        anyhow::bail!("source session is not hosted by the sending mesh member");
    }
    let target = state
        .db
        .get_code_session(target_session_id)?
        .ok_or_else(|| anyhow::anyhow!("target Code session was not found"))?;
    if target.archived_at.is_some() {
        anyhow::bail!("target Code session is archived");
    }
    let host = target
        .host_node
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("target session has no host"))?;
    if host != state.mesh.node_id() {
        return Ok(state
            .mesh
            .rpc(
                host,
                "code.agent_message",
                serde_json::json!({
                    "sourceSessionId": source_session_id,
                    "targetSessionId": target_session_id,
                    "message": message,
                }),
            )
            .await?);
    }
    deliver_agent_message_local(state, from_node, source_session_id, target_session_id, message).await
}

async fn deliver_agent_message_local(
    state: &AppState,
    from_node: &str,
    source_session_id: &str,
    target_session_id: &str,
    message: &str,
) -> anyhow::Result<serde_json::Value> {
    let source = state
        .db
        .get_code_session(source_session_id)?
        .ok_or_else(|| anyhow::anyhow!("source Code session was not found"))?;
    if source.host_node.as_deref() != Some(from_node) || source.archived_at.is_some() {
        anyhow::bail!("source session is not hosted by the sending mesh member");
    }
    let target = state
        .db
        .get_code_session(target_session_id)?
        .ok_or_else(|| anyhow::anyhow!("target Code session was not found"))?;
    if target.archived_at.is_some() {
        anyhow::bail!("target Code session is archived");
    }
    if target.host_node.as_deref() != Some(state.mesh.node_id()) {
        anyhow::bail!("target session is no longer hosted here");
    }
    if state.code.is_moving(target_session_id) {
        anyhow::bail!("target session is being moved to another computer");
    }
    // An idle session may have no live agent process; start it so the message can run.
    if !state.code.is_running(target_session_id) {
        code_start_local(state, target_session_id.to_owned()).await?;
    }
    let source_node = state.mesh.member_name(from_node);
    let text = format!(
        "Patchbay agent message from Code session '{}' ({}) on {} ({}):\n\n{}\n\nTo reply, use the Patchbay send_agent_message tool and target session {}.",
        source.title, source_session_id, source_node, from_node, message, source_session_id
    );
    let prompt_id = new_id();
    let delivery = code_prompt_local(
        state,
        target_session_id.to_owned(),
        text,
        Vec::new(),
        Vec::new(),
        format!("agent:{from_node}:{source_session_id}"),
        Some(prompt_id.clone()),
        false,
    )
    .await?;
    Ok(serde_json::json!({
        "targetSessionId": target_session_id,
        "targetTitle": target.title,
        "delivery": delivery,
        "messageId": prompt_id,
    }))
}

async fn code_unqueue(state: &AppState, id: String, prompt_id: String) -> CmdResult<()> {
    if let Some(host) = remote_host(state, &id)? {
        state.mesh.rpc(&host, "code.unqueue", serde_json::json!({ "id": id, "promptId": prompt_id })).await?;
        return Ok(());
    }
    Ok(state.code.send(&id, AgentCmd::Unqueue { prompt_id })?)
}

/// Whether a `code.attach` answer shows a running agent on a host that can cancel a turn to send
/// a prompt next. Hosts from before force send leave `extensions.forceSend` out.
fn attach_has_force_send(attach: &serde_json::Value) -> bool {
    attach["running"] == true
        && attach["events"].as_array().is_some_and(|evs| {
            evs.iter().any(|e| e["type"] == "sessionReady" && e["extensions"]["forceSend"] == true)
        })
}

/// Refuse a forced send to a remote host that would queue it instead (`code.prompt`) or not know
/// the method (`code.send_now`).
async fn ensure_remote_force_send(state: &AppState, host: &str, id: &str) -> anyhow::Result<()> {
    let attach = state.mesh.rpc(host, "code.attach", serde_json::json!({ "id": id })).await?;
    if attach["running"] != true {
        anyhow::bail!("the agent isn't running on {}", state.mesh.member_name(host));
    }
    if !attach_has_force_send(&attach) {
        anyhow::bail!(
            "{} runs an older Patchbay that can't stop the turn to send this now. Update it, or send the message normally to queue it.",
            state.mesh.member_name(host)
        );
    }
    Ok(())
}

/// Send a queued prompt now: the running turn is cancelled and the prompt goes next.
async fn code_send_now(state: &AppState, id: String, prompt_id: String) -> CmdResult<()> {
    if let Some(host) = remote_host(state, &id)? {
        ensure_remote_force_send(state, &host, &id).await?;
        state.mesh.rpc(&host, "code.send_now", serde_json::json!({ "id": id, "promptId": prompt_id })).await?;
        return Ok(());
    }
    Ok(state.code.send(&id, AgentCmd::SendNow { prompt_id })?)
}

async fn code_prompt(
    state: &AppState,
    id: String,
    text: String,
    attachment_ids: Vec<String>,
    mentions: Option<Vec<String>>,
    prompt_id: Option<String>,
    force: Option<bool>,
) -> CmdResult<()> {
    let mentions = mentions.unwrap_or_default();
    // Cancels the running turn and sends this next (Ctrl+Enter mid-turn).
    let force = force.unwrap_or(false);
    if let Some(host) = remote_host(state, &id)? {
        if force {
            ensure_remote_force_send(state, &host, &id).await?;
        }
        let params = serde_json::json!({
            "id": id, "text": text, "attachmentIds": attachment_ids, "mentions": mentions, "promptId": prompt_id,
            "force": force,
        });
        state.mesh.rpc(&host, "code.prompt", params).await?;
        return Ok(());
    }
    let me = state.mesh.node_id().to_owned();
    code_prompt_local(state, id, text, attachment_ids, mentions, me, prompt_id, force).await?;
    Ok(())
}

async fn code_cancel(state: &AppState, id: String) -> CmdResult<()> {
    if let Some(host) = remote_host(state, &id)? {
        state.mesh.rpc(&host, "code.cancel", serde_json::json!({ "id": id })).await?;
        return Ok(());
    }
    Ok(state.code.send(&id, AgentCmd::Cancel)?)
}

/// Stop one running tool call; the turn goes on.
async fn code_cancel_tool(state: &AppState, id: String, tool_call_id: String) -> CmdResult<()> {
    if let Some(host) = remote_host(state, &id)? {
        state.mesh.rpc(&host, "code.cancel_tool", serde_json::json!({ "id": id, "toolCallId": tool_call_id })).await?;
        return Ok(());
    }
    Ok(state.code.send(&id, AgentCmd::CancelTool { tool_call_id })?)
}

/// Stop one background shell.
async fn code_kill_shell(state: &AppState, id: String, shell_id: String) -> CmdResult<()> {
    if let Some(host) = remote_host(state, &id)? {
        state.mesh.rpc(&host, "code.kill_shell", serde_json::json!({ "id": id, "shellId": shell_id })).await?;
        return Ok(());
    }
    Ok(state.code.send(&id, AgentCmd::KillShell { shell_id })?)
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

/// Where agents keep session state Patchbay can move between computers (see `agent_state`).
fn agent_home(state: &AppState) -> PathBuf {
    state.data_dir.clone()
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
    let (dir, home, kind2) = (state.data_dir.clone(), agent_home(state), kind.to_owned());
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
        let (home, target, from) = (agent_home(state), PathBuf::from(cwd), host_name.clone());
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
    let (dir, home, target) = (state.data_dir.clone(), agent_home(state), PathBuf::from(cwd));
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
                let prompt_id = p["promptId"].as_str().map(str::to_owned);
                let force = p["force"].as_bool().unwrap_or(false);
                code_prompt_local(state, id()?, text, atts, mentions, origin, prompt_id, force)
                    .await
                    .map(|_| Value::Null)
            }
            "code.agent_message" => {
                send_agent_message(
                    state,
                    &from,
                    param(&p, "sourceSessionId")?,
                    param(&p, "targetSessionId")?,
                    param(&p, "message")?,
                )
                .await
            }
            "code.cancel" => {
                ensure_local_host(state, &id()?)?;
                state.code.send(&id()?, AgentCmd::Cancel).map(|_| Value::Null)
            }
            "code.cancel_tool" => {
                ensure_local_host(state, &id()?)?;
                let tool_call_id = param(&p, "toolCallId")?.to_owned();
                state.code.send(&id()?, AgentCmd::CancelTool { tool_call_id }).map(|_| Value::Null)
            }
            "code.kill_shell" => {
                ensure_local_host(state, &id()?)?;
                let shell_id = param(&p, "shellId")?.to_owned();
                state.code.send(&id()?, AgentCmd::KillShell { shell_id }).map(|_| Value::Null)
            }
            "code.unqueue" => {
                ensure_local_host(state, &id()?)?;
                let prompt_id = param(&p, "promptId")?.to_owned();
                state.code.send(&id()?, AgentCmd::Unqueue { prompt_id }).map(|_| Value::Null)
            }
            "code.send_now" => {
                ensure_local_host(state, &id()?)?;
                let prompt_id = param(&p, "promptId")?.to_owned();
                state.code.send(&id()?, AgentCmd::SendNow { prompt_id }).map(|_| Value::Null)
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
            "code.image" => {
                let id = id()?;
                ensure_local_host(state, &id)?;
                let path = param(&p, "path")?.to_owned();
                let s = state.db.get_code_session(&id)?.ok_or_else(|| anyhow::anyhow!("no such session"))?;
                let url = tokio::task::spawn_blocking(move || files::session_image(std::path::Path::new(&s.cwd), &path)).await??;
                Ok(json!(url))
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
            "host.info" => Ok(gateway::host_info_local()),
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
        if name == MESH_PLAN_LIMITS_EVENT {
            use code::plan_limits::PlanLimits;
            let tagged = payload["account"].as_str().zip(serde_json::from_value::<PlanLimits>(payload["limits"].clone()).ok());
            let (account, name, mut l) = match tagged {
                Some((a, l)) => (a.to_owned(), payload["name"].as_str().map(str::to_owned), l),
                // Untagged, from a member before accounts were told apart: the default login's.
                None => {
                    let Ok(l) = serde_json::from_value::<PlanLimits>(payload.clone()) else { return };
                    let (a, n) = default_plan_account();
                    (a, n, l)
                }
            };
            // Their clock may run ahead of ours; a later report seen here must still win.
            l.observed_at = l.observed_at.min(code::plan_limits::now());
            adopt_plan_limits(&self.state, &account, name, l, false);
            return;
        }
        if name == MESH_CURSOR_PLAN_EVENT {
            use code::cursor_plan::CursorPlanLimits;
            let Ok(mut l) = serde_json::from_value::<CursorPlanLimits>(
                payload.get("limits").cloned().unwrap_or_else(|| payload.clone()),
            ) else {
                return;
            };
            if let Some(id) = payload["accountId"].as_str() {
                l.account_id = id.to_owned();
            }
            adopt_cursor_plan(&self.state, l, false);
            return;
        }
        if name != "mesh://checkpoint" {
            return;
        }
        let (Some(id), Some(host)) = (payload["id"].as_str(), payload["host"].as_str()) else { return };
        tokio::spawn(pull_checkpoint(self.state.clone(), id.to_owned(), host.to_owned()));
    }

    fn greeting(&self) -> Vec<(String, serde_json::Value)> {
        let mut out: Vec<(String, serde_json::Value)> = self
            .state
            .code
            .plan
            .accounts()
            .into_iter()
            .filter_map(|(account, a)| {
                let payload = serde_json::json!({ "account": account, "name": a.name, "limits": a.latest? });
                Some((MESH_PLAN_LIMITS_EVENT.to_owned(), payload))
            })
            .collect();
        if let Some(l) = self.state.code.cursor_plan.latest() {
            out.push((
                MESH_CURSOR_PLAN_EVENT.to_owned(),
                serde_json::json!({ "accountId": l.account_id, "limits": l }),
            ));
        }
        out
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
    Ok(state.mesh.approve(&node_id).await?)
}

async fn mesh_reject(state: &AppState, node_id: String) -> CmdResult<()> {
    state.mesh.reject(&node_id).await?;
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
            let mut held = None;
            loop {
                let first = match held.take() {
                    Some(m) => m,
                    None => match rx.recv().await {
                        Some(m) => m,
                        None => break,
                    },
                };
                // Keys typed while the last write was in flight go out as one write, so input
                // lags by one round trip at most instead of one per key.
                let (method, params) = coalesce_term_input(first, &mut rx, &mut held);
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

type TermInput = (String, serde_json::Value);

/// Merge `first`, when it is a write, with the writes already queued behind it. The first
/// queued message that is not a write is left in `held`, to go next.
fn coalesce_term_input(
    first: TermInput,
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<TermInput>,
    held: &mut Option<TermInput>,
) -> TermInput {
    let (method, mut params) = first;
    if method != "term.write" {
        return (method, params);
    }
    let mut data = params["data"].as_str().unwrap_or_default().to_owned();
    while let Ok(next) = rx.try_recv() {
        match next.1["data"].as_str().filter(|_| next.0 == "term.write") {
            Some(more) => data.push_str(more),
            None => {
                *held = Some(next);
                break;
            }
        }
    }
    params["data"] = data.into();
    (method, params)
}

#[cfg(test)]
#[test]
fn coalesce_term_input_merges_writes_in_order() {
    let w = |d: &str| ("term.write".to_owned(), serde_json::json!({ "termId": "t", "data": d }));
    let resize = ("term.resize".to_owned(), serde_json::json!({ "termId": "t", "cols": 100, "rows": 30 }));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    for m in [w("b"), w("c"), resize.clone(), w("d")] {
        tx.send(m).unwrap();
    }
    let mut held = None;
    let (method, params) = coalesce_term_input(w("a"), &mut rx, &mut held);
    assert_eq!((method.as_str(), params["data"].as_str()), ("term.write", Some("abc")));
    assert_eq!(held.take(), Some(resize.clone()));
    let next = rx.try_recv().unwrap();
    assert_eq!(coalesce_term_input(next, &mut rx, &mut held).1["data"], "d");
    assert_eq!(coalesce_term_input(resize.clone(), &mut rx, &mut held), resize);
}

#[cfg(test)]
#[test]
fn force_send_needs_a_host_that_advertises_it() {
    let attach = |running: bool, ext: serde_json::Value| {
        serde_json::json!({ "running": running, "events": [{ "type": "sessionReady", "extensions": ext }] })
    };
    assert!(attach_has_force_send(&attach(true, serde_json::json!({ "steering": true, "forceSend": true }))));
    // A host from before force send: it would queue a forced prompt and not know `code.send_now`.
    assert!(!attach_has_force_send(&attach(true, serde_json::json!({ "steering": true }))));
    assert!(!attach_has_force_send(&attach(false, serde_json::json!({ "forceSend": true }))));
    assert!(!attach_has_force_send(&serde_json::json!({ "running": true, "events": [] })));
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
    // Only the node hosting the session writes the cache. A window viewing a session hosted
    // elsewhere holds a partial view (stale cache plus the live tail), and the cache is a single
    // synced cell, so its save would overwrite the host's complete one on every node.
    if remote_host(state, &id)?.is_some() {
        return Ok(());
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

/// A picture a reply linked, read on the computer that holds the session.
async fn code_session_image(state: &AppState, id: String, path: String) -> CmdResult<String> {
    if let Some(host) = remote_host(state, &id)? {
        let v = state.mesh.rpc(&host, "code.image", serde_json::json!({ "id": id, "path": path })).await?;
        return v.as_str().map(str::to_owned).ok_or_else(|| CmdError("no image".into()));
    }
    let s = state.db.get_code_session(&id)?.ok_or_else(|| CmdError("no such session".into()))?;
    Ok(tokio::task::spawn_blocking(move || files::session_image(std::path::Path::new(&s.cwd), &path)).await??)
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

// ---------------- sidebar layout ----------------

async fn list_sidebar_layout(state: &AppState) -> CmdResult<Vec<SidebarEntry>> {
    Ok(state.db.list_sidebar_layout()?)
}

async fn apply_sidebar_layout(state: &AppState, put: Vec<SidebarEntry>, remove: Vec<String>) -> CmdResult<()> {
    for e in &put {
        if e.key.is_empty() {
            return Err(CmdError("key is required".into()));
        }
        if e.kind != "group" && e.kind != "folder" {
            return Err(CmdError(format!("unknown layout kind {}", e.kind)));
        }
    }
    let put: Vec<SidebarEntry> = put
        .into_iter()
        .map(|mut e| {
            e.name = e.name.trim().to_owned();
            e
        })
        .collect();
    Ok(state.db.apply_sidebar_layout(&put, &remove)?)
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
    if !lock_data_dir(&data_dir)? {
        anyhow::bail!("{}", data_dir_busy(&data_dir));
    }
    open_locked_state(data_dir, emit)
}

/// Take this process's lock on `data_dir`, held until it exits. One process per data folder: two
/// would share a mesh identity and fight over sessions. False when another process (the app or
/// patchbayd) holds it; a desktop app then attaches to a daemon that holds it (see ipc.rs).
pub fn lock_data_dir(data_dir: &std::path::Path) -> anyhow::Result<bool> {
    use anyhow::Context as _;
    std::fs::create_dir_all(data_dir)?;
    let lock = std::fs::File::create(data_dir.join("instance.lock"))?;
    match lock.try_lock() {
        Ok(()) => {
            std::mem::forget(lock);
            Ok(true)
        }
        Err(std::fs::TryLockError::WouldBlock) => Ok(false),
        Err(std::fs::TryLockError::Error(e)) => Err(e).context("locking the data folder"),
    }
}

/// Why a data folder can't be opened while another process holds its lock.
pub fn data_dir_busy(data_dir: &std::path::Path) -> String {
    format!("another Patchbay (the app or patchbayd) is already using {}", data_dir.display())
}

/// `open_state` for a caller that already holds the folder's lock (`lock_data_dir`).
pub fn open_locked_state(data_dir: PathBuf, emit: mesh::node::UiSink) -> anyhow::Result<AppState> {
    let db = Arc::new(Db::open(&data_dir.join("patchbay.sqlite"))?);
    let keys = Arc::new(mesh::keys::OsKeyStore::new(&data_dir));
    let mesh = Mesh::new(db.clone(), data_dir.clone(), keys)?;
    let state = AppState::new(db, data_dir, mesh.clone(), emit);
    let (agent_messaging, message_listener) = agent_messaging::AgentMessagingBridge::bind()?;
    let _ = state.agent_message_bridge.set(agent_messaging.clone());
    agent_messaging.serve(message_listener, state.clone());
    if mesh.is_member() {
        api::spawn(async move { mesh.start() });
    }
    let sweeper = state.clone();
    api::spawn(async move {
        loop {
            if let Err(e) = auto_archive_sweep(&sweeper).await {
                tracing::warn!("auto-archive: {e:#}");
            }
            tokio::time::sleep(AUTO_ARCHIVE_EVERY).await;
        }
    });
    let resumer = state.clone();
    api::spawn(async move {
        loop {
            tokio::time::sleep(RESUME_EVERY).await;
            resume_after_limit(&resumer).await;
            resume_after_cursor_limit(&resumer).await;
        }
    });
    let cursor_poller = state.clone();
    api::spawn(async move {
        loop {
            tokio::time::sleep(code::cursor_plan::TICK).await;
            poll_cursor_plan(&cursor_poller).await;
        }
    });
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
