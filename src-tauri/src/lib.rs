mod attachments;
mod chat;
mod code;
mod db;
mod desktop;
mod files;
mod prefs;
mod providers;
mod terminal;
mod usage;
#[cfg(test)]
mod agent_live_tests;
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
use crate::terminal::{TermEvent, TerminalManager};

pub struct AppState {
    db: Arc<Db>,
    data_dir: PathBuf,
    chat: Arc<ChatManager>,
    code: Arc<CodeManager>,
    terminals: TerminalManager,
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
    let ch = on_event.clone();
    // Run detached so the invoke resolves immediately; progress flows over the channel.
    tauri::async_runtime::spawn(async move {
        if let Err(e) =
            chat::send(db, data_dir, chat, conversation_id, model_ref, turn, reasoning_effort, on_event).await
        {
            let _ = ch.send(ChatEvent::Error {
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

#[tauri::command]
fn cancel_chat(state: State<'_, AppState>, conversation_id: String) {
    state.chat.cancel(&conversation_id);
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

/// Start (or restart) the agent for a code session. Output arrives as `code://event`.
#[tauri::command]
async fn code_start(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    id: String,
) -> CmdResult<()> {
    use tauri::Emitter;
    let sink: code::acp::Sink = Arc::new(move |env| {
        let _ = app.emit(code::acp::EVENT, env);
    });
    let (code, db, dir) = (state.code.clone(), state.db.clone(), state.data_dir.clone());
    // `start` may block briefly while a previous agent for this session shuts down.
    tokio::task::spawn_blocking(move || code.start(sink, db, &dir, &id)).await??;
    Ok(())
}

#[tauri::command]
fn code_is_running(state: State<'_, AppState>, id: String) -> bool {
    state.code.is_running(&id)
}

#[tauri::command]
fn code_stop(state: State<'_, AppState>, id: String) {
    state.code.stop(&id);
}

#[tauri::command]
async fn code_prompt(
    state: State<'_, AppState>,
    id: String,
    text: String,
    attachment_ids: Vec<String>,
    mentions: Option<Vec<String>>,
) -> CmdResult<()> {
    let mut blocks = Vec::new();
    // @-mentioned project files go as standard ACP resource links; the agent reads them itself.
    for path in mentions.unwrap_or_default() {
        let p = std::path::Path::new(&path);
        let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.clone());
        blocks.push(serde_json::json!({ "type": "resource_link", "name": name, "uri": attachments::file_uri(p) }));
    }
    for aid in &attachment_ids {
        let att = state
            .db
            .get_attachment(aid)?
            .ok_or_else(|| CmdError("attachment not found".into()))?;
        let dir = state.data_dir.clone();
        blocks.extend(
            tokio::task::spawn_blocking(move || attachments::to_acp_blocks(&dir, &att)).await??,
        );
    }
    if !text.is_empty() {
        blocks.push(serde_json::json!({ "type": "text", "text": text }));
    }
    // Bumps updated_at so the session sorts to the top of the sidebar.
    state.db.update_code_session(&id, None, None, None)?;
    Ok(state.code.send(&id, AgentCmd::Prompt { blocks })?)
}

#[tauri::command]
fn code_cancel(state: State<'_, AppState>, id: String) -> CmdResult<()> {
    Ok(state.code.send(&id, AgentCmd::Cancel)?)
}

#[tauri::command]
async fn code_set_model(
    state: State<'_, AppState>,
    id: String,
    model_id: String,
) -> CmdResult<()> {
    state.code.set_model(&id, model_id.clone()).await?;
    let session = state.db.get_code_session(&id)?;
    let patchbay_models = session
        .as_ref()
        .and_then(|s| code::agents::AgentKind::parse(&s.agent))
        .is_some_and(|k| k.uses_patchbay_models());
    if patchbay_models {
        if let Some(r) = code::grok_config::model_ref_from_key(&model_id) {
            state.db.update_code_session(&id, None, None, Some(r))?;
        }
    } else {
        // Remembered so the next agent process for this session reselects it.
        state.db.set_code_session_agent_model(&id, &model_id)?;
    }
    Ok(())
}

#[tauri::command]
async fn code_set_config_option(
    state: State<'_, AppState>,
    id: String,
    config_id: String,
    value: String,
) -> CmdResult<()> {
    Ok(state.code.set_config_option(&id, config_id, value).await?)
}

#[tauri::command]
async fn code_set_mode(state: State<'_, AppState>, id: String, mode_id: String) -> CmdResult<()> {
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
    Ok(state.code.rewind(&id, ordinal, preview, revert_files).await?)
}

#[tauri::command]
fn code_usage(state: State<'_, AppState>, id: String) -> CmdResult<usage::UsageView> {
    Ok(state.db.code_usage(&id)?.view())
}

#[tauri::command]
fn code_permission_respond(
    state: State<'_, AppState>,
    request_id: String,
    option_id: Option<String>,
) {
    state.code.respond_permission(&request_id, option_id);
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

#[tauri::command]
async fn list_project_files(cwd: String) -> CmdResult<Vec<String>> {
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
    Ok(code::mcp::save(&state.db, input.id, &input.name, input.enabled, input.config)?)
}

#[tauri::command]
fn delete_mcp_server(state: State<'_, AppState>, id: String) -> CmdResult<()> {
    Ok(code::mcp::delete(&state.db, &id)?)
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
            app.manage(AppState {
                db,
                data_dir,
                chat: Arc::default(),
                code: Arc::default(),
                terminals: TerminalManager::default(),
            });
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
        ])
        .build(tauri::generate_context!())
        .expect("error while building Patchbay")
        .run(|app, event| {
            if let tauri::RunEvent::Exit = event
                && let Some(state) = app.try_state::<AppState>()
            {
                state.code.stop_all();
                state.terminals.close_all();
            }
        });
}
