//! Desktop integration: tray icon, the quick-entry window and its global shortcut, and the main
//! window's title bar.

use std::str::FromStr;

use anyhow::{Result, anyhow};
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutState};

pub const QUICK: &str = "quick";
/// Main window events: `tray://new-chat`, `quick://prompt` (payload: the text).
pub const EV_NEW_CHAT: &str = "tray://new-chat";
pub const EV_QUICK_PROMPT: &str = "quick://prompt";

pub fn show_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
    }
}

/// `title_bar` preference: "system" keeps the window manager's title bar; "app" removes it and
/// the UI draws its own window controls.
pub fn apply_title_bar(app: &AppHandle, title_bar: &str) {
    let Some(w) = app.get_webview_window("main") else {
        return;
    };
    let app_drawn = title_bar == "app";
    if let Err(e) = w.set_decorations(!app_drawn) {
        tracing::warn!("title bar: {e:#}");
    }
    #[cfg(target_os = "linux")]
    {
        let win = w.clone();
        if let Err(e) = w.run_on_main_thread(move || announce_wayland_decorations(&win, app_drawn)) {
            tracing::warn!("title bar: {e:#}");
        }
    }
}

/// Wayland compositors with server-side decorations (KWin) keep drawing their title bar unless
/// the window says it decorates itself. GTK only says so when a window is first shown, so
/// `set_decorations` alone changes nothing there; tell the compositor directly, which also
/// works on a window that is already open. X11 and GNOME follow `set_decorations` on their own.
#[cfg(target_os = "linux")]
fn announce_wayland_decorations(w: &tauri::WebviewWindow, app_drawn: bool) {
    use gtk::glib::object::ObjectExt;
    use gtk::glib::translate::ToGlibPtr;
    use gtk::prelude::WidgetExt;

    let Ok(gtk_window) = w.gtk_window() else {
        return;
    };
    let Some(gdk_window) = gtk_window.window() else {
        return;
    };
    if gdk_window.type_().name() != "GdkWaylandWindow" {
        return;
    }
    let ptr: *mut gtk::gdk::ffi::GdkWindow = gdk_window.to_glib_none().0;
    // SAFETY: `ptr` is a live GdkWaylandWindow (type checked above), borrowed for the call,
    // on the main thread.
    unsafe {
        if app_drawn {
            gdk_wayland_sys::gdk_wayland_window_announce_csd(ptr.cast());
        } else {
            gdk_wayland_sys::gdk_wayland_window_announce_ssd(ptr.cast());
        }
    }
}

pub fn toggle_quick(app: &AppHandle) {
    if let Some(w) = app.get_webview_window(QUICK) {
        if w.is_visible().unwrap_or(false) {
            let _ = w.hide();
        } else {
            let _ = w.center();
            let _ = w.show();
            let _ = w.set_focus();
        }
        return;
    }
    let built = WebviewWindowBuilder::new(app, QUICK, WebviewUrl::App("index.html?quick=1".into()))
        .title("Patchbay")
        .inner_size(640.0, 132.0)
        .resizable(false)
        .decorations(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .center()
        .focused(true)
        .build();
    if let Err(e) = built {
        tracing::warn!("quick entry window: {e}");
    }
}

pub fn hide_quick(app: &AppHandle) {
    if let Some(w) = app.get_webview_window(QUICK) {
        let _ = w.hide();
    }
}

/// Replace the global quick-entry shortcut. Empty disables it.
pub fn set_quick_shortcut(app: &AppHandle, accel: &str) -> Result<()> {
    let gs = app.global_shortcut();
    let accel = accel.trim();
    let next = if accel.is_empty() {
        None
    } else {
        Some(Shortcut::from_str(accel).map_err(|e| anyhow!("invalid shortcut {accel}: {e}"))?)
    };
    gs.unregister_all().map_err(|e| anyhow!("{e}"))?;
    if let Some(s) = next {
        gs.on_shortcut(s, |app, _s, ev| {
            if ev.state == ShortcutState::Pressed {
                toggle_quick(app);
            }
        })
        .map_err(|e| anyhow!("could not register {accel}: {e}"))?;
    }
    Ok(())
}

pub fn validate_shortcut(accel: &str) -> std::result::Result<(), String> {
    let accel = accel.trim();
    if accel.is_empty() {
        return Ok(());
    }
    Shortcut::from_str(accel).map(|_| ()).map_err(|e| format!("invalid shortcut {accel}: {e}"))
}

pub fn build_tray(app: &AppHandle) -> Result<()> {
    let show = MenuItem::with_id(app, "show", "Show Patchbay", true, None::<&str>)?;
    let new_chat = MenuItem::with_id(app, "new-chat", "New chat", true, None::<&str>)?;
    let quick = MenuItem::with_id(app, "quick", "Quick entry…", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let sep = PredefinedMenuItem::separator(app)?;
    let menu = Menu::with_items(app, &[&show, &new_chat, &quick, &sep, &quit])?;
    let mut builder = TrayIconBuilder::with_id("main")
        .tooltip("Patchbay")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, ev| match ev.id().as_ref() {
            "show" => show_main(app),
            "new-chat" => {
                show_main(app);
                let _ = app.emit_to("main", EV_NEW_CHAT, ());
            }
            "quick" => toggle_quick(app),
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, ev| {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = ev {
                show_main(tray.app_handle());
            }
        });
    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }
    builder.build(app)?;
    Ok(())
}

/// `DesktopHooks` for the native app.
struct Hooks(AppHandle);

impl crate::DesktopHooks for Hooks {
    fn validate_shortcut(&self, accel: &str) -> std::result::Result<(), String> {
        validate_shortcut(accel)
    }

    fn set_quick_shortcut(&self, accel: &str) -> Result<()> {
        set_quick_shortcut(&self.0, accel)
    }

    fn apply_title_bar(&self, title_bar: &str) {
        apply_title_bar(&self.0, title_bar)
    }
}

// ---------------- desktop-only commands ----------------

/// From the quick-entry window: hand the text to the main window as a new chat.
#[tauri::command]
fn quick_submit(app: AppHandle, text: String) -> crate::CmdResult<()> {
    hide_quick(&app);
    show_main(&app);
    app.emit_to("main", EV_QUICK_PROMPT, text)?;
    Ok(())
}

#[tauri::command]
fn quick_dismiss(app: AppHandle) {
    hide_quick(&app);
}

/// Write a user-chosen export (the path comes from the native save dialog).
#[tauri::command]
fn write_text_file(path: String, contents: String) -> crate::CmdResult<()> {
    Ok(std::fs::write(&path, contents)?)
}

/// Voices of the operating system on this computer. Empty when it has none.
#[tauri::command]
fn list_native_voices() -> Vec<crate::speech::NativeVoice> {
    crate::speech::list_native_voices()
}

/// Speak on this computer's speakers. The browser UI never calls this.
/// Blocking on a worker so Stop can kill the process while a sentence is still playing.
#[tauri::command]
async fn speak_native(text: String, voice: String, rate: String) -> crate::CmdResult<()> {
    let joined = tauri::async_runtime::spawn_blocking(move || crate::speech::speak_native(&text, &voice, &rate)).await;
    match joined {
        Ok(r) => Ok(r?),
        Err(e) => Err(crate::CmdError(e.to_string())),
    }
}

#[tauri::command]
fn stop_native() {
    crate::speech::stop_native();
}

/// Serve an invoke from the window. Shared commands go through `api::call`, the same dispatcher
/// the daemon's web clients use; the rest are the desktop-only commands above.
fn invoke_handler() -> impl Fn(tauri::ipc::Invoke) -> bool + Send + Sync + 'static {
    let desktop_only: Box<dyn Fn(tauri::ipc::Invoke) -> bool + Send + Sync> =
        Box::new(tauri::generate_handler![
            quick_submit,
            quick_dismiss,
            write_text_file,
            list_native_voices,
            speak_native,
            stop_native
        ]);
    move |invoke| {
        if crate::api::COMMANDS.contains(&invoke.message.command()) {
            dispatch(invoke);
            true
        } else {
            desktop_only(invoke)
        }
    }
}

/// Run a shared command for the window, answering through the invoke's resolver.
fn dispatch<R: tauri::Runtime>(invoke: tauri::ipc::Invoke<R>) {
    let cmd = invoke.message.command().to_owned();
    let webview = invoke.message.webview();
    let state = webview.state::<crate::AppState>().inner().clone();
    let args = match invoke.message.payload() {
        tauri::ipc::InvokeBody::Json(v) => v.clone(),
        tauri::ipc::InvokeBody::Raw(_) => serde_json::Value::Null,
    };
    let chans: crate::api::ChanFactory = std::sync::Arc::new(move |id: &str| {
        let id: tauri::ipc::JavaScriptChannelId = id.parse().ok()?;
        let ch = id.channel_on::<_, serde_json::Value>(webview.clone());
        Some(std::sync::Arc::new(move |v| ch.send(v).is_ok()) as crate::api::ChanSend)
    });
    let resolver = invoke.resolver;
    tauri::async_runtime::spawn(async move {
        match crate::api::call(&state, &cmd, args, &chans).await {
            Ok(v) => resolver.resolve(v),
            Err(e) => resolver.reject(e.0),
        }
    });
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    crate::init_tracing();

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .setup(|app| {
            let emit: crate::mesh::node::UiSink = {
                let h = app.handle().clone();
                std::sync::Arc::new(move |name: &str, payload: serde_json::Value| {
                    let _ = h.emit(name, payload);
                })
            };
            let state = match crate::open_state(app.path().app_data_dir()?, emit) {
                Ok(s) => s,
                Err(e) => {
                    use tauri_plugin_dialog::{DialogExt, MessageDialogKind};
                    app.dialog()
                        .message(format!("{e:#}"))
                        .title("Patchbay can't start")
                        .kind(MessageDialogKind::Error)
                        .blocking_show();
                    std::process::exit(1);
                }
            };
            let handle = app.handle().clone();
            let _ = state.desktop.set(std::sync::Arc::new(Hooks(handle.clone())));
            let p = crate::prefs::load(&state.db);
            app.manage(state);
            // A tray needs a StatusNotifier host on Linux; never let its absence stop the app.
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| build_tray(&handle))) {
                Ok(Err(e)) => tracing::warn!("tray icon unavailable: {e:#}"),
                Err(_) => tracing::warn!("tray icon unavailable"),
                Ok(Ok(())) => {}
            }
            if p.title_bar != "system" {
                apply_title_bar(&handle, &p.title_bar);
            }
            if let Err(e) = set_quick_shortcut(&handle, &p.quick_entry_shortcut) {
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
        .invoke_handler(invoke_handler())
        .build(tauri::generate_context!())
        .expect("error while building Patchbay")
        .run(|app, event| {
            if let tauri::RunEvent::Exit = event
                && let Some(state) = app.try_state::<crate::AppState>()
            {
                state.shutdown();
            }
        });
}

/// The window's IPC path, on Tauri's mock runtime: `cargo test --lib --features ipc-test`.
#[cfg(all(test, feature = "ipc-test"))]
mod ipc_tests {
    use serde_json::{Value, json};
    use tauri::Manager;
    use tauri::ipc::{CallbackFn, InvokeBody};
    use tauri::test::{INVOKE_KEY, get_ipc_response, mock_builder, mock_context, noop_assets};
    use tauri::webview::InvokeRequest;

    fn invoke(w: &tauri::WebviewWindow<tauri::test::MockRuntime>, cmd: &str, args: Value) -> Result<Value, Value> {
        let req = InvokeRequest {
            cmd: cmd.into(),
            callback: CallbackFn(0),
            error: CallbackFn(1),
            url: "tauri://localhost".parse().unwrap(),
            body: InvokeBody::Json(args),
            headers: Default::default(),
            invoke_key: INVOKE_KEY.to_string(),
        };
        get_ipc_response(w, req).map(|b| b.deserialize::<Value>().unwrap())
    }

    #[test]
    fn window_commands_go_through_the_dispatcher() {
        let dir = std::env::temp_dir().join(format!("pb-ipc-{}", crate::db::new_id()));
        let app = mock_builder()
            .invoke_handler(|i| {
                super::dispatch(i);
                true
            })
            .build(mock_context(noop_assets()))
            .unwrap();
        let emit: crate::mesh::node::UiSink = std::sync::Arc::new(|_: &str, _: Value| {});
        app.manage(crate::open_state(dir.clone(), emit).unwrap());
        let w = tauri::WebviewWindowBuilder::new(&app, "main", Default::default()).build().unwrap();

        let p = invoke(&w, "save_prompt", json!({ "name": "Greet", "content": "Say hi" })).unwrap();
        assert_eq!(p["name"], "Greet");
        let list = invoke(&w, "list_prompts", json!({})).unwrap();
        assert_eq!(list.as_array().unwrap().len(), 1);
        let err = invoke(&w, "list_messages", json!({})).unwrap_err();
        assert!(err.as_str().unwrap().contains("conversationId"), "{err}");
        let err = invoke(&w, "save_prompt", json!({ "name": " ", "content": "" })).unwrap_err();
        assert_eq!(err, "name is required");
        let _ = std::fs::remove_dir_all(dir);
    }
}
