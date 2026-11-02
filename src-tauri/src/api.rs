//! The command dispatcher. The desktop window (through Tauri's IPC) and the daemon's web clients
//! (through a WebSocket) both call commands by name with a JSON object of camelCase arguments, as
//! `invoke(name, args)` sends them, and get JSON back.

use std::cell::RefCell;
use std::marker::PhantomData;
use std::sync::{Arc, OnceLock};

use serde::de::{DeserializeOwned, Error as _};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::*;

/// Delivers one message to a frontend channel; false once the other end is gone.
pub type ChanSend = Arc<dyn Fn(Value) -> bool + Send + Sync>;
/// Opens the frontend channel named by an id (`__CHANNEL__:<n>`, a serialized JS `Channel`).
pub type ChanFactory = Arc<dyn Fn(&str) -> Option<ChanSend> + Send + Sync>;

/// A frontend `Channel` passed as a command argument: a stream of events back to the caller.
pub struct Chan<T> {
    send: ChanSend,
    _t: PhantomData<fn(T)>,
}

impl<T> Clone for Chan<T> {
    fn clone(&self) -> Self {
        Chan { send: self.send.clone(), _t: PhantomData }
    }
}

impl<T: Serialize> Chan<T> {
    /// False once the frontend has gone away.
    pub fn send(&self, ev: T) -> bool {
        match serde_json::to_value(ev) {
            Ok(v) => (self.send)(v),
            Err(_) => false,
        }
    }
}

thread_local! {
    /// The caller's channel factory while its arguments deserialize (see `arg`).
    static CHANS: RefCell<Option<ChanFactory>> = const { RefCell::new(None) };
}

impl<'de, T> Deserialize<'de> for Chan<T> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let id = String::deserialize(d)?;
        let send = CHANS
            .with(|c| c.borrow().as_ref().and_then(|f| f(&id)))
            .ok_or_else(|| D::Error::custom("invalid channel"))?;
        Ok(Chan { send, _t: PhantomData })
    }
}

/// Take argument `name` (snake_case here, camelCase on the wire) out of `args`. A missing key
/// reads as null, so `Option` arguments may be left out and required ones fail with its name.
fn arg<T: DeserializeOwned>(args: &mut Value, name: &str, chans: &ChanFactory) -> CmdResult<T> {
    let key = camel(name);
    let v = args.get_mut(&key).map(Value::take).unwrap_or(Value::Null);
    CHANS.with(|c| *c.borrow_mut() = Some(chans.clone()));
    let r = serde_json::from_value(v);
    CHANS.with(|c| *c.borrow_mut() = None);
    r.map_err(|e| CmdError(format!("invalid argument {key}: {e}")))
}

fn camel(snake: &str) -> String {
    let mut out = String::with_capacity(snake.len());
    let mut up = false;
    for c in snake.chars() {
        if c == '_' {
            up = true;
        } else if up {
            out.extend(c.to_uppercase());
            up = false;
        } else {
            out.push(c);
        }
    }
    out
}

macro_rules! commands {
    ($($name:ident($($a:ident),*);)*) => {
        /// Every command `call` serves.
        pub const COMMANDS: &[&str] = &[$(stringify!($name)),*];

        /// Run command `cmd` with `args` (a JSON object).
        #[allow(unused_mut, unused_variables)]
        pub async fn call(state: &AppState, cmd: &str, mut args: Value, chans: &ChanFactory) -> CmdResult<Value> {
            match cmd {
                $(stringify!($name) => {
                    let r = $name(state, $(arg(&mut args, stringify!($a), chans)?),*).await?;
                    Ok(serde_json::to_value(r)?)
                })*
                other => Err(CmdError(format!("unknown command {other}"))),
            }
        }
    };
}

commands! {
    list_providers();
    save_provider(input);
    delete_provider(id);
    discover_models(provider_id);
    list_models();
    save_model(input);
    delete_model(id);
    get_preferences();
    set_preferences(prefs);
    synthesize_speech(provider_id, base_url, model, voice, text);
    rewrite_for_speech(model_ref, text);
    get_setting(key);
    set_setting(key, value);
    web_search_status();
    claude_overview(cwd);
    set_web_search_key(key);
    clear_web_search_key();
    list_conversations();
    create_conversation(model_ref);
    update_conversation(id, title, model_ref, system_prompt, pinned, archived);
    delete_conversation(id);
    list_messages(conversation_id);
    switch_branch(conversation_id, message_id);
    search_messages(query, limit);
    send_chat_message(conversation_id, model_ref, text, attachment_ids, parent_id, reasoning_effort, on_event);
    regenerate_reply(conversation_id, model_ref, user_message_id, reasoning_effort, on_event);
    cancel_chat(conversation_id);
    add_attachment_path(path);
    add_attachment_bytes(name, data_base64);
    attachment_thumbnail(id);
    attachment_image(id);
    list_code_sessions();
    create_code_session(cwd, model_ref, agent, host);
    list_agents(host);
    list_claude_sessions();
    import_claude_sessions(ids, agent);
    update_code_session(id, title, model_ref, pinned, activity, archived);
    delete_code_session(id);
    code_auto_title(id);
    code_start(id);
    code_attach(id);
    code_resume(id, point, running);
    code_is_running(id);
    code_stop(id);
    code_prompt(id, text, attachment_ids, mentions, prompt_id, force);
    code_unqueue(id, prompt_id);
    code_send_now(id, prompt_id);
    code_cancel(id);
    code_cancel_tool(id, tool_call_id);
    code_kill_shell(id, shell_id);
    code_set_model(id, model_id);
    code_set_config_option(id, config_id, value);
    code_authenticate(id, method_id, api_key);
    code_set_agent(id, agent, model_ref);
    code_set_mode(id, mode_id);
    code_set_permission_mode(id, mode);
    code_rewind(id, ordinal, preview, revert_files);
    code_usage(id);
    plan_accounts();
    cursor_plan_limits();
    cursor_plan_watching(open);
    session_plan_usage(id);
    wire_log_status();
    wire_log_set(on);
    wire_log_frames(session, last);
    wire_log_clear();
    wire_log_save();
    code_permission_respond(request_id, option_id, id);
    code_session_image(id, path);
    code_move(id, target, cwd);
    code_take_over(id, cwd);
    code_checkpoints();
    mesh_status();
    mesh_create(passphrase, name);
    mesh_join(url, passphrase, name);
    mesh_cancel_join();
    mesh_approve(node_id);
    mesh_reject(node_id);
    mesh_remove(node_id);
    mesh_leave();
    mesh_set_config(name, listen, advertise);
    mesh_add_address(url);
    mesh_set_passphrase(passphrase);
    mesh_chat_streams();
    term_open(id, cwd, cols, rows, on_event, session_id, auth_method);
    term_write(id, data);
    term_resize(id, cols, rows);
    term_close(id);
    get_code_transcript(id);
    save_code_transcript(id, json);
    list_project_files(cwd, session_id);
    list_mcp_servers();
    save_mcp_server(input);
    delete_mcp_server(id);
    list_permission_rules();
    add_permission_rule(cwd, tool_kind, pattern);
    delete_permission_rule(id);
    list_sidebar_layout();
    apply_sidebar_layout(put, remove);
    list_prompts();
    save_prompt(id, name, content);
    delete_prompt(id);
    usage_report(days);
    list_dirs(path, host);
    new_session_options(host);
    host_info(host);
}

/// The Patchbay version and operating system of this computer, or with `host` of that computer.
async fn host_info(state: &AppState, host: Option<String>) -> CmdResult<Value> {
    match host.filter(|h| h != state.mesh.node_id()) {
        Some(h) => Ok(gateway::on_host(state, &serde_json::json!({ "host": h }), "host.info").await?),
        None => Ok(gateway::host_info_local()),
    }
}

/// Folders under `path` (default: home), for choosing a project folder without a native dialog;
/// with `host`, on that computer.
async fn list_dirs(state: &AppState, path: Option<String>, host: Option<String>) -> CmdResult<Value> {
    match host.filter(|h| h != state.mesh.node_id()) {
        Some(h) => Ok(gateway::on_host(state, &serde_json::json!({ "host": h, "path": path }), "fs.dirs").await?),
        None => Ok(gateway::list_dirs_local(path.as_deref())?),
    }
}

/// What starting a Code session here, or with `host` on that computer, offers: its installed
/// agents, default agent, home folder and recent folders.
async fn new_session_options(state: &AppState, host: Option<String>) -> CmdResult<Value> {
    match host.filter(|h| h != state.mesh.node_id()) {
        Some(h) => Ok(gateway::on_host(state, &serde_json::json!({ "host": h }), "code.new_options").await?),
        None => Ok(gateway::new_session_options_local(state).await?),
    }
}

/// The runtime background work runs on; the daemon sets it at startup.
pub static RUNTIME: OnceLock<tokio::runtime::Handle> = OnceLock::new();

/// Spawn a task from anywhere, including threads outside the async runtime.
pub fn spawn<F>(fut: F)
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    #[cfg(feature = "desktop")]
    if RUNTIME.get().is_none() {
        tauri::async_runtime::spawn(fut);
        return;
    }
    match tokio::runtime::Handle::try_current().ok().or_else(|| RUNTIME.get().cloned()) {
        Some(h) => {
            h.spawn(fut);
        }
        None => tracing::error!("no async runtime to spawn on"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn camel_case_keys() {
        assert_eq!(camel("provider_id"), "providerId");
        assert_eq!(camel("data_base64"), "dataBase64");
        assert_eq!(camel("id"), "id");
    }

    #[test]
    fn args_read_by_camel_case_name() {
        let chans: ChanFactory = Arc::new(|_| None);
        let mut args = serde_json::json!({ "conversationId": "c1", "limit": 5 });
        let id: String = arg(&mut args, "conversation_id", &chans).unwrap();
        let missing: Option<String> = arg(&mut args, "parent_id", &chans).unwrap();
        assert_eq!(id, "c1");
        assert_eq!(missing, None);
        let err = arg::<String>(&mut args, "model_ref", &chans).unwrap_err();
        assert!(err.0.contains("modelRef"), "{}", err.0);
    }

    #[test]
    fn channels_come_from_the_callers_factory() {
        let got = Arc::new(std::sync::Mutex::new(Vec::new()));
        let g = got.clone();
        let chans: ChanFactory = Arc::new(move |id| {
            let g = g.clone();
            let id = id.to_owned();
            Some(Arc::new(move |v: Value| {
                g.lock().unwrap().push((id.clone(), v));
                true
            }) as ChanSend)
        });
        let mut args = serde_json::json!({ "onEvent": "__CHANNEL__:7" });
        let ch: Chan<u32> = arg(&mut args, "on_event", &chans).unwrap();
        assert!(ch.send(3));
        assert_eq!(got.lock().unwrap()[0], ("__CHANNEL__:7".to_owned(), serde_json::json!(3)));
    }
}
