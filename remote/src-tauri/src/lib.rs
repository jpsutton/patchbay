//! Patchbay Remote, the phone app. Its backend answers the commands the desktop app's backend
//! does, by asking the gateway it is connected to (see `patchbay_mesh::client`), so the desktop's
//! chat and Code views run here unchanged. Design: docs/design/mobile.md.

mod keystore;

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine;
use patchbay_mesh::client::{self, RemoteClient};
use patchbay_mesh::remote as rmt;
use serde::Serialize;
use serde_json::{Value, json};
use tauri::ipc::Channel;
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_notification::NotificationExt;
use tauri_plugin_remote_platform::PlatformExt;

/// Chat topics followed at once; the oldest is dropped beyond this.
const MAX_CHAT_TOPICS: usize = 12;
/// Raw bytes per attachment upload chunk.
const UPLOAD_CHUNK: usize = 3 * 1024 * 1024;
/// How long the lists stay fresh without a change notice from the gateway.
const INDEX_TTL: Duration = Duration::from_secs(20);
const CATALOG_TTL: Duration = Duration::from_secs(120);

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
type Shared<'a> = State<'a, Arc<Remote>>;

struct Remote {
    client: RemoteClient,
    app: AppHandle,
    dir: PathBuf,
    /// Chat replies started here: conversation -> the asking window's channel.
    channels: Mutex<HashMap<String, Channel<Value>>>,
    /// Chat topics followed, oldest first.
    chats: Mutex<VecDeque<String>>,
    index: Mutex<Option<(Instant, Value)>>,
    /// Bumped whenever the index goes stale, so a load already in flight doesn't cache it.
    index_gen: AtomicU64,
    catalog: Mutex<Option<(Instant, Value)>>,
    /// This phone's own settings and preferences.
    local: Mutex<serde_json::Map<String, Value>>,
    foreground: AtomicBool,
}

impl Remote {
    fn emit(&self, name: &str, payload: Value) {
        let _ = self.app.emit(name, payload);
    }

    fn save_local(&self) {
        let s = serde_json::to_string_pretty(&*self.local.lock().unwrap()).unwrap_or_default();
        let _ = std::fs::write(self.dir.join("local.json"), s);
    }

    /// The computer picked to run a chat's turns (`key`: its id, or "new" before it exists).
    /// `None`: the gateway runs them.
    fn chat_node(&self, key: &str) -> Option<String> {
        self.local.lock().unwrap().get("chatNodes")?.get(key)?.as_str().map(str::to_owned)
    }

    fn set_chat_node(&self, key: &str, node: Option<String>) {
        {
            let mut local = self.local.lock().unwrap();
            let nodes = local.entry("chatNodes").or_insert_with(|| json!({}));
            match node {
                Some(n) => nodes[key] = json!(n),
                None => {
                    if let Some(m) = nodes.as_object_mut() {
                        m.remove(key);
                    }
                }
            }
        }
        self.save_local();
    }

    /// Notify only while the app is out of sight; on screen, the views show it.
    fn notify(&self, title: &str, body: &str) {
        if self.foreground.load(Ordering::Relaxed) {
            return;
        }
        if let Err(e) = self.app.notification().builder().title(title).body(body).show() {
            tracing::debug!("notification: {e}");
        }
    }

    fn title_of(&self, list: &str, id: &str) -> String {
        self.index
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|(_, v)| v[list].as_array()?.iter().find(|x| x["id"] == id).map(|x| x["title"].as_str().unwrap_or("").to_owned()))
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| if list == "codeSessions" { "Code session".into() } else { "Chat".into() })
    }

    fn on_event(&self, name: &str, payload: Value) {
        match name {
            rmt::EV_CHAT => {
                let conv = payload["conversationId"].as_str().unwrap_or_default().to_owned();
                let ev = payload["event"].clone();
                let kind = ev["type"].as_str().unwrap_or_default().to_owned();
                if kind == "done" {
                    self.notify("Reply ready", &self.title_of("conversations", &conv));
                }
                let ch = self.channels.lock().unwrap().get(&conv).cloned();
                if let Some(ch) = ch {
                    let _ = ch.send(ev);
                    if kind == "done" || kind == "error" {
                        self.channels.lock().unwrap().remove(&conv);
                    }
                    return;
                }
                self.emit("mesh://chat", payload);
            }
            rmt::EV_CODE => {
                let id = payload["codeSessionId"].as_str().unwrap_or_default();
                match payload["event"]["type"].as_str() {
                    Some("permission") => self.notify("Agent needs your approval", &self.title_of("codeSessions", id)),
                    Some("promptDone") if payload["event"]["stopReason"] != "cancelled" => {
                        self.notify("Agent finished", &self.title_of("codeSessions", id))
                    }
                    _ => {}
                }
                self.emit("code://event", payload);
            }
            rmt::EV_LIVE => {
                // Sessions not open here still need you when their agent asks.
                let id = payload["id"].as_str().unwrap_or_default();
                let followed = self.client.topics().contains(&rmt::session_topic(id));
                if !followed && payload["live"]["permissions"].as_array().is_some_and(|p| !p.is_empty()) {
                    self.notify("Agent needs your approval", &self.title_of("codeSessions", id));
                }
                self.emit("remote://live", payload);
            }
            rmt::EV_INDEX => {
                self.forget_index();
                self.emit("mesh://changed", json!({ "tables": ["conversations", "messages", "code_sessions"] }));
            }
            client::EV_STATUS | client::EV_MEMBERS => {
                self.emit("remote://status", self.client.status());
            }
            client::EV_RESYNC => self.emit("remote://resync", payload),
            _ => {}
        }
    }

    async fn index(&self) -> CmdResult<Value> {
        if let Some((t, v)) = self.index.lock().unwrap().as_ref()
            && t.elapsed() < INDEX_TTL
        {
            return Ok(v.clone());
        }
        let generation = self.index_gen.load(Ordering::SeqCst);
        let v = self.client.rpc("remote.index", Value::Null).await?;
        let mut cache = self.index.lock().unwrap();
        if self.index_gen.load(Ordering::SeqCst) == generation {
            *cache = Some((Instant::now(), v.clone()));
        }
        Ok(v)
    }

    /// Drop the cached index: the next load asks the gateway again.
    fn forget_index(&self) {
        let mut cache = self.index.lock().unwrap();
        self.index_gen.fetch_add(1, Ordering::SeqCst);
        *cache = None;
    }

    async fn catalog(&self) -> CmdResult<Value> {
        if let Some((t, v)) = self.catalog.lock().unwrap().as_ref()
            && t.elapsed() < CATALOG_TTL
        {
            return Ok(v.clone());
        }
        let v = self.client.rpc("remote.catalog", Value::Null).await?;
        *self.catalog.lock().unwrap() = Some((Instant::now(), v.clone()));
        Ok(v)
    }

    /// Follow a conversation's live stream, keeping only the most recent few.
    async fn follow_chat(&self, conv: &str) {
        let topic = rmt::chat_topic(conv);
        let dropped = {
            let mut q = self.chats.lock().unwrap();
            q.retain(|t| t != &topic);
            q.push_back(topic.clone());
            if q.len() > MAX_CHAT_TOPICS { q.pop_front() } else { None }
        };
        if let Some(old) = dropped {
            self.client.unsubscribe(&old);
        }
        if !self.client.topics().contains(&topic)
            && let Err(e) = self.client.subscribe(&topic, false).await
        {
            tracing::debug!("following {conv}: {e:#}");
        }
    }

    async fn code(&self, method: &str, params: Value) -> CmdResult<Value> {
        Ok(self.client.rpc("remote.code", json!({ "method": method, "params": params })).await?)
    }

    async fn upload(&self, name: &str, bytes: &[u8]) -> CmdResult<Value> {
        let id = hex::encode(rand::random::<[u8; 16]>());
        let b64 = base64::engine::general_purpose::STANDARD;
        let chunks: Vec<&[u8]> = if bytes.is_empty() { vec![&[][..]] } else { bytes.chunks(UPLOAD_CHUNK).collect() };
        let mut offset = 0usize;
        let mut last = Value::Null;
        for (i, c) in chunks.iter().enumerate() {
            let done = i + 1 == chunks.len();
            let p = json!({ "uploadId": id, "name": name, "offset": offset, "data": b64.encode(c), "done": done });
            last = self.client.rpc("remote.attach_put", p).await?;
            offset += c.len();
        }
        Ok(last)
    }
}

// ---------------- the phone's own commands ----------------

#[tauri::command]
fn remote_status(r: Shared<'_>) -> Value {
    r.client.status()
}

#[tauri::command]
fn remote_join(r: Shared<'_>, url: String, passphrase: String, name: String) -> CmdResult<()> {
    Ok(r.client.join(&url, &passphrase, &name)?)
}

#[tauri::command]
fn remote_cancel_join(r: Shared<'_>) {
    r.client.cancel_join();
}

#[tauri::command]
fn remote_leave(r: Shared<'_>) {
    r.client.leave();
    r.forget_index();
    *r.catalog.lock().unwrap() = None;
}

/// The network may have changed (the app came back, or the user asked to retry).
#[tauri::command]
fn remote_kick(r: Shared<'_>) {
    r.client.kick();
}

#[tauri::command]
fn remote_set_foreground(r: Shared<'_>, foreground: bool) {
    r.foreground.store(foreground, Ordering::Relaxed);
    r.client.set_foreground(foreground);
}

/// Stop following a session or conversation the window no longer shows.
#[tauri::command]
async fn remote_unwatch(r: Shared<'_>, topic: String) -> CmdResult<()> {
    if topic != rmt::TOPIC_INDEX {
        r.client.unsubscribe(&topic);
    }
    Ok(())
}

/// Keep the connection open in the background (Android's foreground service). Async, so it runs
/// off the main thread, which the plugin call waits on.
#[tauri::command]
async fn remote_keep_alive(r: Shared<'_>, on: bool) -> CmdResult<()> {
    let st = r.client.status();
    let text = match st["gatewayName"].as_str() {
        Some(g) => format!("Connected through {g}"),
        None => "Reconnecting to your mesh".into(),
    };
    r.app.platform().keep_alive(on, &text).map_err(CmdError)
}

/// Ask for the phone's fingerprint, face or screen lock. True when it passed (or none exists).
#[tauri::command]
async fn remote_unlock(app: AppHandle, reason: String) -> CmdResult<bool> {
    #[cfg(mobile)]
    {
        use tauri_plugin_biometric::{AuthOptions, BiometricExt};
        if app.biometric().status().map(|s| !s.is_available).unwrap_or(true) {
            return Ok(true);
        }
        let opts = AuthOptions { allow_device_credential: true, ..Default::default() };
        return Ok(app.biometric().authenticate(reason, opts).is_ok());
    }
    #[cfg(not(mobile))]
    {
        let _ = (app, reason);
        Ok(true)
    }
}

// ---------------- the desktop's commands, answered through the gateway ----------------

#[tauri::command]
async fn mesh_status(r: Shared<'_>) -> CmdResult<Value> {
    let mut st = r.client.status();
    st["listen"] = json!("");
    st["port"] = Value::Null;
    st["advertise"] = json!([]);
    Ok(st)
}

#[tauri::command]
async fn list_providers(r: Shared<'_>) -> CmdResult<Value> {
    Ok(r.catalog().await?["providers"].clone())
}

#[tauri::command]
async fn list_models(r: Shared<'_>) -> CmdResult<Value> {
    Ok(r.catalog().await?["models"].clone())
}

#[tauri::command]
async fn get_setting(r: Shared<'_>, key: String) -> CmdResult<Option<String>> {
    if let Some(v) = r.local.lock().unwrap().get(&format!("setting:{key}")).and_then(Value::as_str) {
        return Ok(Some(v.to_owned()));
    }
    if key == "default_model" {
        return Ok(r.catalog().await?["defaultModel"].as_str().map(str::to_owned));
    }
    Ok(None)
}

#[tauri::command]
fn set_setting(r: Shared<'_>, key: String, value: String) {
    r.local.lock().unwrap().insert(format!("setting:{key}"), Value::String(value));
    r.save_local();
}

#[tauri::command]
async fn get_preferences(r: Shared<'_>) -> CmdResult<Value> {
    let mut p = r.local.lock().unwrap().get("prefs").cloned().unwrap_or_else(|| json!({}));
    if p.get("defaultSystemPrompt").is_none()
        && let Ok(c) = r.catalog().await
    {
        p["defaultSystemPrompt"] = c["defaultSystemPrompt"].clone();
    }
    Ok(p)
}

#[tauri::command]
fn set_preferences(r: Shared<'_>, prefs: Value) {
    r.local.lock().unwrap().insert("prefs".into(), prefs);
    r.save_local();
}

#[tauri::command]
async fn list_conversations(r: Shared<'_>) -> CmdResult<Value> {
    Ok(r.index().await?["conversations"].clone())
}

#[tauri::command]
async fn create_conversation(r: Shared<'_>, model_ref: Option<String>) -> CmdResult<Value> {
    r.forget_index();
    let conv = r.client.rpc("remote.create_conversation", json!({ "modelRef": model_ref })).await?;
    // The computer picked for the new chat runs this one.
    if let (Some(id), Some(node)) = (conv["id"].as_str(), r.chat_node("new")) {
        r.set_chat_node(id, Some(node));
        r.set_chat_node("new", None);
    }
    Ok(conv)
}

/// The computer picked to run a chat (`key`: its id, or "new"); `None` = the gateway.
#[tauri::command]
fn remote_chat_node(r: Shared<'_>, key: String) -> Option<String> {
    r.chat_node(&key)
}

#[tauri::command]
fn remote_set_chat_node(r: Shared<'_>, key: String, node: Option<String>) {
    r.set_chat_node(&key, node);
}

/// Starting a Code session on a chosen computer: its agents, default agent and recent folders.
#[tauri::command]
async fn remote_new_session_options(r: Shared<'_>, host: String) -> CmdResult<Value> {
    Ok(r.client.rpc("remote.new_session_options", json!({ "host": host })).await?)
}

/// The folders in `path` on `host` (its home folder when `path` is missing).
#[tauri::command]
async fn remote_list_dirs(r: Shared<'_>, host: String, path: Option<String>) -> CmdResult<Value> {
    Ok(r.client.rpc("remote.list_dirs", json!({ "host": host, "path": path })).await?)
}

#[tauri::command]
async fn remote_create_code_session(
    r: Shared<'_>,
    host: String,
    cwd: String,
    model_ref: Option<String>,
    agent: Option<String>,
) -> CmdResult<Value> {
    r.forget_index();
    let p = json!({ "host": host, "cwd": cwd, "modelRef": model_ref, "agent": agent });
    Ok(r.client.rpc("remote.create_code_session", p).await?)
}

#[tauri::command]
async fn update_conversation(
    r: Shared<'_>,
    id: String,
    title: Option<String>,
    model_ref: Option<String>,
    system_prompt: Option<String>,
    pinned: Option<bool>,
    archived: Option<bool>,
) -> CmdResult<()> {
    r.forget_index();
    let p = json!({
        "id": id, "title": title, "modelRef": model_ref, "systemPrompt": system_prompt, "pinned": pinned, "archived": archived,
    });
    r.client.rpc("remote.update_conversation", p).await?;
    Ok(())
}

#[tauri::command]
fn delete_conversation() -> CmdResult<()> {
    Err(CmdError("delete conversations on a computer".into()))
}

#[tauri::command]
async fn list_messages(r: Shared<'_>, conversation_id: String) -> CmdResult<Value> {
    let msgs = r.client.rpc("remote.messages", json!({ "conversationId": conversation_id })).await?;
    let r2 = r.inner().clone();
    tokio::spawn(async move { r2.follow_chat(&conversation_id).await });
    Ok(msgs)
}

#[tauri::command]
async fn switch_branch(r: Shared<'_>, conversation_id: String, message_id: String) -> CmdResult<()> {
    r.client.rpc("remote.switch_branch", json!({ "conversationId": conversation_id, "messageId": message_id })).await?;
    Ok(())
}

#[tauri::command]
fn search_messages() -> Vec<Value> {
    Vec::new()
}

#[tauri::command]
#[allow(clippy::too_many_arguments)]
async fn send_chat_message(
    r: Shared<'_>,
    conversation_id: String,
    model_ref: String,
    text: String,
    attachment_ids: Vec<String>,
    parent_id: Option<String>,
    reasoning_effort: Option<String>,
    on_event: Channel<Value>,
) -> CmdResult<()> {
    r.follow_chat(&conversation_id).await;
    r.channels.lock().unwrap().insert(conversation_id.clone(), on_event);
    let mut p = json!({
        "conversationId": conversation_id,
        "modelRef": model_ref,
        "text": text,
        "attachmentIds": attachment_ids,
        "reasoningEffort": reasoning_effort,
        "node": r.chat_node(&conversation_id),
    });
    // Absent = after the visible branch; "" = a new first message; an id = under that one.
    if let Some(parent) = parent_id {
        p["parentId"] = json!(parent);
    }
    if let Err(e) = r.client.rpc("remote.chat_send", p).await {
        r.channels.lock().unwrap().remove(&conversation_id);
        return Err(e.into());
    }
    Ok(())
}

#[tauri::command]
async fn regenerate_reply(
    r: Shared<'_>,
    conversation_id: String,
    model_ref: String,
    user_message_id: String,
    reasoning_effort: Option<String>,
    on_event: Channel<Value>,
) -> CmdResult<()> {
    r.follow_chat(&conversation_id).await;
    r.channels.lock().unwrap().insert(conversation_id.clone(), on_event);
    let p = json!({
        "conversationId": conversation_id,
        "modelRef": model_ref,
        "userMessageId": user_message_id,
        "reasoningEffort": reasoning_effort,
        "node": r.chat_node(&conversation_id),
    });
    if let Err(e) = r.client.rpc("remote.chat_regenerate", p).await {
        r.channels.lock().unwrap().remove(&conversation_id);
        return Err(e.into());
    }
    Ok(())
}

#[tauri::command]
async fn cancel_chat(r: Shared<'_>, conversation_id: String) -> CmdResult<()> {
    r.client.rpc("remote.chat_cancel", json!({ "conversationId": conversation_id })).await?;
    Ok(())
}

#[tauri::command]
async fn mesh_chat_streams(r: Shared<'_>) -> CmdResult<Value> {
    let me = r.client.node_id();
    let all = r.client.rpc("remote.streams", Value::Null).await?;
    // Replies the gateway runs for this phone come back through their channel instead.
    let mine: BTreeSet<String> = r.channels.lock().unwrap().keys().cloned().collect();
    let list: Vec<Value> = all
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|s| s["streamer"] != me.as_str() && !s["conversationId"].as_str().is_some_and(|c| mine.contains(c)))
        .collect();
    Ok(json!(list))
}

#[tauri::command]
async fn add_attachment_bytes(r: Shared<'_>, name: String, data_base64: String) -> CmdResult<Value> {
    let bytes = base64::engine::general_purpose::STANDARD.decode(data_base64)?;
    r.upload(&name, &bytes).await
}

#[tauri::command]
async fn add_attachment_path(r: Shared<'_>, path: String) -> CmdResult<Value> {
    let p = PathBuf::from(&path);
    let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "attachment".into());
    let bytes = std::fs::read(&p)?;
    r.upload(&name, &bytes).await
}

#[tauri::command]
async fn attachment_thumbnail(r: Shared<'_>, id: String) -> CmdResult<Value> {
    Ok(r.client.rpc("remote.thumbnail", json!({ "id": id })).await?)
}

#[tauri::command]
async fn list_code_sessions(r: Shared<'_>) -> CmdResult<Value> {
    Ok(r.index().await?["codeSessions"].clone())
}

/// Live state of every Code session (status, mid-turn, open permission requests).
#[tauri::command]
async fn remote_live(r: Shared<'_>) -> CmdResult<Value> {
    Ok(r.index().await?["live"].clone())
}

#[tauri::command]
async fn update_code_session(
    r: Shared<'_>,
    id: String,
    title: Option<String>,
    pinned: Option<bool>,
    archived: Option<bool>,
) -> CmdResult<()> {
    if title.is_some() || pinned.is_some() || archived.is_some() {
        r.forget_index();
        let p = json!({ "id": id, "title": title, "pinned": pinned, "archived": archived });
        r.client.rpc("remote.update_code_session", p).await?;
    }
    Ok(())
}

#[tauri::command]
async fn code_auto_title(r: Shared<'_>, id: String) -> CmdResult<Value> {
    r.forget_index();
    r.code("code.auto_title", json!({ "id": id })).await
}

/// The agents installed on `host`, the computer running a session; a phone runs none itself.
#[tauri::command]
async fn list_agents(r: Shared<'_>, host: Option<String>) -> CmdResult<Value> {
    let Some(host) = host else { return Ok(json!([])) };
    let options = r.client.rpc("remote.new_session_options", json!({ "host": host })).await?;
    Ok(options["agents"].clone())
}

#[tauri::command]
async fn get_code_transcript(r: Shared<'_>, id: String) -> CmdResult<Value> {
    Ok(r.client.rpc("remote.transcript", json!({ "id": id })).await?)
}

/// The transcript cache belongs to the computers; a phone's view doesn't overwrite it.
#[tauri::command]
fn save_code_transcript() {}

#[tauri::command]
fn code_checkpoints() -> Value {
    json!({})
}

/// Follow the session and catch up: the host's replay plus what happened since its last turn.
#[tauri::command]
async fn code_attach(r: Shared<'_>, id: String) -> CmdResult<Value> {
    let res = r.client.subscribe(&rmt::session_topic(&id), true).await?;
    let attach = &res["snapshot"]["attach"];
    let mut events: Vec<Value> = attach["events"].as_array().cloned().unwrap_or_default();
    let tail = res["tail"].as_array().cloned().unwrap_or_default();
    if res["covered"] == false && !tail.is_empty() {
        events.push(json!({ "type": "notice", "message": "Earlier output from this turn isn't available here" }));
    }
    events.extend(tail.iter().map(|e| e[1]["event"].clone()));
    Ok(json!({
        "running": attach["running"].as_bool().unwrap_or(false),
        "busy": attach["busy"].as_bool().unwrap_or(false),
        "offline": attach["offline"].as_bool().unwrap_or(false),
        "events": events,
        "point": res["point"],
    }))
}

#[tauri::command]
async fn code_start(r: Shared<'_>, id: String) -> CmdResult<()> {
    r.code("code.start", json!({ "id": id })).await?;
    Ok(())
}

#[tauri::command]
async fn code_is_running(r: Shared<'_>, id: String) -> CmdResult<bool> {
    Ok(r.code("code.is_running", json!({ "id": id })).await?.as_bool().unwrap_or(false))
}

#[tauri::command]
async fn code_set_agent(r: Shared<'_>, id: String, agent: String, model_ref: Option<String>) -> CmdResult<()> {
    r.forget_index();
    r.code("code.set_agent", json!({ "id": id, "agent": agent, "modelRef": model_ref })).await?;
    Ok(())
}

#[tauri::command]
async fn code_stop(r: Shared<'_>, id: String) -> CmdResult<()> {
    r.code("code.stop", json!({ "id": id })).await?;
    Ok(())
}

#[tauri::command]
async fn code_prompt(
    r: Shared<'_>,
    id: String,
    text: String,
    attachment_ids: Vec<String>,
    mentions: Option<Vec<String>>,
    prompt_id: Option<String>,
) -> CmdResult<()> {
    // The host echoes the prompt back under this id; the view already shows it under the same one.
    let p = json!({
        "id": id, "text": text, "attachmentIds": attachment_ids, "mentions": mentions.unwrap_or_default(),
        "promptId": prompt_id,
    });
    r.code("code.prompt", p).await?;
    Ok(())
}

#[tauri::command]
async fn code_cancel(r: Shared<'_>, id: String) -> CmdResult<()> {
    r.code("code.cancel", json!({ "id": id })).await?;
    Ok(())
}

#[tauri::command]
async fn code_set_model(r: Shared<'_>, id: String, model_id: String) -> CmdResult<()> {
    r.code("code.set_model", json!({ "id": id, "modelId": model_id })).await?;
    Ok(())
}

#[tauri::command]
async fn code_set_mode(r: Shared<'_>, id: String, mode_id: String) -> CmdResult<()> {
    r.code("code.set_mode", json!({ "id": id, "modeId": mode_id })).await?;
    Ok(())
}

#[tauri::command]
async fn code_set_permission_mode(r: Shared<'_>, id: String, mode: String) -> CmdResult<()> {
    r.code("code.set_permission_mode", json!({ "id": id, "mode": mode })).await?;
    Ok(())
}

#[tauri::command]
async fn code_set_config_option(r: Shared<'_>, id: String, config_id: String, value: String) -> CmdResult<()> {
    r.code("code.set_config_option", json!({ "id": id, "configId": config_id, "value": value })).await?;
    Ok(())
}

#[tauri::command]
async fn code_rewind(r: Shared<'_>, id: String, ordinal: u64, preview: String, revert_files: bool) -> CmdResult<()> {
    let p = json!({ "id": id, "ordinal": ordinal, "preview": preview, "revertFiles": revert_files });
    r.code("code.rewind", p).await?;
    Ok(())
}

#[tauri::command]
async fn code_permission_respond(
    r: Shared<'_>,
    request_id: String,
    option_id: Option<String>,
    id: Option<String>,
) -> CmdResult<()> {
    let id = id.ok_or_else(|| CmdError("which session?".into()))?;
    r.code("code.permission", json!({ "id": id, "requestId": request_id, "optionId": option_id })).await?;
    Ok(())
}

#[tauri::command]
async fn code_usage(r: Shared<'_>, id: String) -> CmdResult<Value> {
    Ok(r.client.rpc("remote.usage", json!({ "id": id })).await?)
}

#[tauri::command]
async fn list_project_files(r: Shared<'_>, session_id: Option<String>) -> CmdResult<Value> {
    match session_id {
        Some(id) => r.code("code.files", json!({ "id": id })).await,
        None => Ok(json!([])),
    }
}

/// Tell the client when any network interface's address changes (Wi-Fi to cellular, a VPN
/// coming up), so it checks its connection at once instead of waiting for pings to fail.
fn watch_network(client: RemoteClient) {
    let addrs = || -> BTreeSet<String> {
        if_addrs::get_if_addrs()
            .map(|v| v.into_iter().filter(|i| !i.is_loopback()).map(|i| format!("{}/{}", i.name, i.ip())).collect())
            .unwrap_or_default()
    };
    tauri::async_runtime::spawn(async move {
        let mut last = addrs();
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            let now = addrs();
            if now != last {
                tracing::info!("network changed");
                last = now;
                client.kick();
            }
        }
    });
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_env("PATCHBAY_LOG").unwrap_or_else(|_| "info".into()))
        .try_init();

    #[cfg_attr(not(mobile), allow(unused_mut))]
    let mut builder = tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_remote_platform::init());
    #[cfg(mobile)]
    {
        builder = builder.plugin(tauri_plugin_biometric::init());
    }
    builder
        .setup(|app| {
            let dir = app.path().app_data_dir()?;
            std::fs::create_dir_all(&dir)?;
            let (h1, h2) = (app.handle().clone(), app.handle().clone());
            let keys = Arc::new(keystore::SealedKeyStore::new(
                dir.join("keys.json"),
                Box::new(move |v| h1.platform().seal(v)),
                Box::new(move |v| h2.platform().open(v)),
            ));
            let handle = app.handle().clone();
            let sink: client::Sink = Arc::new(move |name: &str, payload: Value| {
                if let Some(r) = handle.try_state::<Arc<Remote>>() {
                    r.on_event(name, payload);
                }
            });
            let client = RemoteClient::new(dir.join("mesh"), keys, sink)?;
            let local = std::fs::read_to_string(dir.join("local.json"))
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_default();
            app.manage(Arc::new(Remote {
                client: client.clone(),
                app: app.handle().clone(),
                dir,
                channels: Mutex::default(),
                chats: Mutex::default(),
                index: Mutex::default(),
                index_gen: AtomicU64::new(0),
                catalog: Mutex::default(),
                local: Mutex::new(local),
                foreground: AtomicBool::new(true),
            }));
            tauri::async_runtime::spawn(async move { client.start() });
            watch_network(app.state::<Arc<Remote>>().client.clone());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            remote_status,
            remote_join,
            remote_cancel_join,
            remote_leave,
            remote_kick,
            remote_set_foreground,
            remote_unwatch,
            remote_keep_alive,
            remote_unlock,
            remote_live,
            mesh_status,
            list_providers,
            list_models,
            get_setting,
            set_setting,
            get_preferences,
            set_preferences,
            list_conversations,
            create_conversation,
            remote_chat_node,
            remote_set_chat_node,
            remote_new_session_options,
            remote_list_dirs,
            remote_create_code_session,
            update_conversation,
            delete_conversation,
            list_messages,
            switch_branch,
            search_messages,
            send_chat_message,
            regenerate_reply,
            cancel_chat,
            mesh_chat_streams,
            add_attachment_bytes,
            add_attachment_path,
            attachment_thumbnail,
            list_code_sessions,
            update_code_session,
            code_auto_title,
            list_agents,
            get_code_transcript,
            save_code_transcript,
            code_checkpoints,
            code_attach,
            code_start,
            code_is_running,
            code_stop,
            code_set_agent,
            code_prompt,
            code_cancel,
            code_set_model,
            code_set_mode,
            code_set_permission_mode,
            code_set_config_option,
            code_rewind,
            code_permission_respond,
            code_usage,
            list_project_files,
        ])
        .run(tauri::generate_context!())
        .expect("error while running Patchbay Remote");
}
