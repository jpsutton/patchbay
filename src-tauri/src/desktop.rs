//! Desktop integration: tray icon, the quick-entry window and its global shortcut, and the main
//! window's title bar.

use std::str::FromStr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutState};

pub const QUICK: &str = "quick";
/// Main window events: `tray://new-chat`, `quick://prompt` (payload: the text),
/// `notification://open` (payload: the `open` extra of the clicked notification).
pub const EV_NEW_CHAT: &str = "tray://new-chat";
pub const EV_QUICK_PROMPT: &str = "quick://prompt";
pub const EV_NOTIFICATION_OPEN: &str = "notification://open";

/// Where the main window was when the tray hid it. The window manager treats a window that is
/// shown again like a new one and places it by its own policy (KWin centers it), so put it back.
static MAIN_HIDDEN_AT: Mutex<Option<tauri::PhysicalPosition<i32>>> = Mutex::new(None);

pub fn show_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        if let Some(pos) = MAIN_HIDDEN_AT.lock().ok().and_then(|mut p| p.take())
            && !w.is_visible().unwrap_or(true)
            && !w.is_maximized().unwrap_or(false)
        {
            let _ = w.set_position(pos);
        }
        let _ = w.show();
        let _ = w.set_focus();
    }
}

/// When the main window last lost focus. Some panels take focus on a tray click, so the window
/// is already unfocused when the click arrives; a blur just before the click still counts.
static MAIN_BLURRED_AT: Mutex<Option<Instant>> = Mutex::new(None);
const BLUR_GRACE: Duration = Duration::from_millis(500);

fn main_recently_focused() -> bool {
    MAIN_BLURRED_AT.lock().ok().and_then(|t| *t).is_some_and(|t| t.elapsed() < BLUR_GRACE)
}

/// Tray icon left click: hide the main window when it is in front, otherwise bring it forward
/// (from hidden, minimized or behind other windows).
pub fn toggle_main(app: &AppHandle) {
    let Some(w) = app.get_webview_window("main") else {
        return;
    };
    let in_front = w.is_visible().unwrap_or(false)
        && !w.is_minimized().unwrap_or(false)
        && (w.is_focused().unwrap_or(false) || main_recently_focused());
    if in_front {
        if let (Ok(pos), Ok(mut saved)) = (w.outer_position(), MAIN_HIDDEN_AT.lock()) {
            *saved = Some(pos);
        }
        let _ = w.hide();
    } else {
        show_main(app);
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

/// Register the saved quick-entry shortcut at startup: through the GlobalShortcuts portal on a
/// Wayland session, else (or when the portal is unavailable) through the plugin.
fn init_quick_shortcut(app: &AppHandle, accel: &str) {
    #[cfg(target_os = "linux")]
    if crate::portal_shortcuts::wayland_session() {
        crate::portal_shortcuts::start(app.clone(), accel, register_quick_shortcut);
        return;
    }
    register_quick_shortcut(app, accel);
}

fn register_quick_shortcut(app: &AppHandle, accel: &str) {
    if let Err(e) = set_quick_shortcut(app, accel) {
        tracing::warn!("quick entry shortcut: {e:#}");
    }
}

/// Replace the global quick-entry shortcut. Empty disables it. On a Wayland session the
/// GlobalShortcuts portal handles it when available (see `portal_shortcuts`).
pub fn set_quick_shortcut(app: &AppHandle, accel: &str) -> Result<()> {
    let accel = accel.trim();
    let next = if accel.is_empty() {
        None
    } else {
        Some(Shortcut::from_str(accel).map_err(|e| anyhow!("invalid shortcut {accel}: {e}"))?)
    };
    #[cfg(target_os = "linux")]
    if crate::portal_shortcuts::update(accel) {
        return Ok(());
    }
    let gs = app.global_shortcut();
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
                toggle_main(tray.app_handle());
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

// ---------------- file links ----------------

/// A path as an agent wrote it, made absolute: `~/…` against home, relative ones against the
/// session's folder. Relative paths without a folder to resolve them against give `None`.
fn resolve_mentioned(path: &str, cwd: Option<&str>) -> Option<std::path::PathBuf> {
    let p = std::path::Path::new(path);
    if let Some(rest) = path.strip_prefix("~/") {
        return crate::code::agents::home_dir().map(|h| h.join(rest));
    }
    if p.is_absolute() {
        return Some(p.to_path_buf());
    }
    cwd.filter(|c| !c.is_empty()).map(|c| std::path::Path::new(c).join(p))
}

/// Something the OS would run rather than show: an executable file, a Windows script or
/// installer, or a macOS app bundle. Agents write these too, so a click reveals them instead.
fn runs_when_opened(p: &std::path::Path, meta: &std::fs::Metadata) -> bool {
    let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    if meta.is_dir() {
        return ext == "app";
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o111 != 0 {
            return true;
        }
    }
    matches!(
        ext.as_str(),
        "exe" | "bat" | "cmd" | "com" | "ps1" | "vbs" | "vbe" | "js" | "jse" | "wsf" | "wsh" | "msi" | "msc"
            | "scr" | "lnk" | "url" | "hta" | "cpl" | "reg" | "desktop" | "command" | "jar"
    )
}

/// Whether a path mentioned in a session's transcript exists on this computer, so the view only
/// links what a click can open.
#[tauri::command]
fn local_path_exists(path: String, cwd: Option<String>) -> bool {
    resolve_mentioned(&path, cwd.as_deref()).is_some_and(|p| p.exists())
}

/// Open a mentioned file or folder in the OS's default app. Anything that would run is shown in
/// the file manager instead.
#[tauri::command]
fn open_local_path(path: String, cwd: Option<String>) -> crate::CmdResult<()> {
    let p = resolve_mentioned(&path, cwd.as_deref()).ok_or_else(|| anyhow!("{path} is relative and has no folder"))?;
    let meta = std::fs::metadata(&p).map_err(|e| anyhow!("{}: {e}", p.display()))?;
    if runs_when_opened(&p, &meta) {
        tauri_plugin_opener::reveal_item_in_dir(&p)?;
    } else {
        tauri_plugin_opener::open_path(&p, None::<&str>)?;
    }
    Ok(())
}

// ---------------- updates ----------------

/// A newer release, found by `update_check` and kept until `update_install` applies it.
#[derive(Default)]
struct PendingUpdate(tokio::sync::Mutex<Option<tauri_plugin_updater::Update>>);

#[derive(serde::Serialize)]
struct UpdateInfo {
    version: String,
    notes: Option<String>,
}

/// Ask the release feed (`plugins.updater` in tauri.conf.json) for a newer version. Only builds
/// that can replace themselves look: the macOS app, the Windows installers and the AppImage.
/// .deb, .rpm and Arch installs update through their package manager.
#[tauri::command]
async fn update_check(
    app: AppHandle,
    pending: tauri::State<'_, PendingUpdate>,
) -> crate::CmdResult<Option<UpdateInfo>> {
    use tauri_plugin_updater::UpdaterExt;
    if cfg!(target_os = "linux") && std::env::var_os("APPIMAGE").is_none() {
        return Ok(None);
    }
    let update = app.updater()?.check().await?;
    let info = update.as_ref().map(|u| UpdateInfo { version: u.version.clone(), notes: u.body.clone() });
    *pending.0.lock().await = update;
    Ok(info)
}

/// Download and install the update `update_check` found, then restart into it. The Windows
/// installer exits the app itself.
#[tauri::command]
async fn update_install(app: AppHandle, pending: tauri::State<'_, PendingUpdate>) -> crate::CmdResult<()> {
    let update = pending.0.lock().await.take().ok_or_else(|| anyhow!("no update to install"))?;
    update.download_and_install(|_, _| {}, || {}).await?;
    app.request_restart();
    Ok(())
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
            local_path_exists,
            open_local_path,
            update_check,
            update_install,
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

/// Run a shared command for the window, answering through the invoke's resolver: on this app's
/// own backend, or on the patchbayd it attached to (see daemon_link.rs).
fn dispatch<R: tauri::Runtime>(invoke: tauri::ipc::Invoke<R>) {
    let cmd = invoke.message.command().to_owned();
    let webview = invoke.message.webview();
    let state = webview.try_state::<crate::AppState>().map(|s| s.inner().clone());
    let link = crate::daemon_link::attached(&webview);
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
        let r = match (state, link) {
            (Some(state), _) => crate::api::call(&state, &cmd, args, &chans).await.map_err(|e| e.0),
            (None, Some(link)) if cmd == "set_preferences" => link.set_preferences(args, &chans).await,
            (None, Some(link)) => link.call(&cmd, args, &chans).await,
            (None, None) => Err("Patchbay is still starting".to_owned()),
        };
        match r {
            Ok(v) => resolver.resolve(v),
            Err(e) => resolver.reject(e),
        }
    });
}

/// Start this app's backend: its own on the data folder, or the patchbayd that already holds the
/// folder. Returns the saved title bar and quick-entry shortcut, or what to tell the user.
fn start_backend(app: &AppHandle, data_dir: std::path::PathBuf) -> std::result::Result<(String, String), String> {
    if crate::lock_data_dir(&data_dir).map_err(|e| format!("{e:#}"))? {
        let emit: crate::mesh::node::UiSink = {
            let h = app.clone();
            std::sync::Arc::new(move |name: &str, payload: serde_json::Value| {
                let _ = h.emit(name, payload);
            })
        };
        let state = crate::open_locked_state(data_dir, emit).map_err(|e| format!("{e:#}"))?;
        let _ = state.desktop.set(std::sync::Arc::new(Hooks(app.clone())));
        let p = crate::prefs::load(&state.db);
        app.manage(state);
        return Ok((p.title_bar, p.quick_entry_shortcut));
    }
    match crate::daemon_link::DaemonLink::attach(app, data_dir.clone()) {
        Ok(link) => {
            tracing::info!("attached to the patchbayd running on {}", data_dir.display());
            let prefs = link.startup_prefs();
            app.manage(link);
            Ok(prefs)
        }
        Err(crate::ipc::client::AttachError::Unreachable(_)) => Err(crate::data_dir_busy(&data_dir)),
        Err(e) => Err(format!("{}\n\n{e}", crate::data_dir_busy(&data_dir))),
    }
}

/// GDK backends to try, in order, when `GDK_BACKEND` is not set. X11 comes first so a Wayland
/// session runs the app on XWayland. Native Wayland lets the compositor refuse to raise the
/// window on a tray click (no xdg-activation token reaches us), places the window anew each
/// time it is shown again, and ignores the quick-entry window's centering, always-on-top and
/// skip-taskbar. Under XWayland all of these work. Unlike setting `GDK_BACKEND`, this does not
/// leak into child processes (terminals, agents), and a user's `GDK_BACKEND=wayland` still wins.
#[cfg(target_os = "linux")]
const GDK_BACKENDS: &str = "x11,wayland,*";

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Must run before GTK opens its display, which happens when the Tauri app is built.
    #[cfg(target_os = "linux")]
    gtk::gdk::set_allowed_backends(GDK_BACKENDS);
    crate::init_tracing();

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_clipboard_manager::init())
        .manage(PendingUpdate::default())
        .setup(|app| {
            let handle = app.handle().clone();
            let (title_bar, quick_entry_shortcut) = match start_backend(&handle, app.path().app_data_dir()?) {
                Ok(p) => p,
                Err(message) => {
                    use tauri_plugin_dialog::{DialogExt, MessageDialogKind};
                    app.dialog()
                        .message(message)
                        .title("Patchbay can't start")
                        .kind(MessageDialogKind::Error)
                        .blocking_show();
                    std::process::exit(1);
                }
            };
            // A tray needs a StatusNotifier host on Linux; never let its absence stop the app.
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| build_tray(&handle))) {
                Ok(Err(e)) => tracing::warn!("tray icon unavailable: {e:#}"),
                Err(_) => tracing::warn!("tray icon unavailable"),
                Ok(Ok(())) => {}
            }
            // A click on one of our notifications raises the window and opens what it was about.
            {
                use tauri_plugin_notification::NotificationExt;
                let h = handle.clone();
                let _ = app.notification().on_action(move |a| {
                    if a.action_id() != "tap" {
                        return;
                    }
                    show_main(&h);
                    if let Some(open) = a.notification().and_then(|n| n.extra().get("open")) {
                        let _ = h.emit_to("main", EV_NOTIFICATION_OPEN, open.clone());
                    }
                });
            }
            if title_bar != "system" {
                apply_title_bar(&handle, &title_bar);
            }
            init_quick_shortcut(&handle, &quick_entry_shortcut);
            Ok(())
        })
        .on_window_event(|w, ev| {
            // The hidden quick-entry window must not keep the app alive after the main one closes.
            if w.label() == "main" && matches!(ev, tauri::WindowEvent::Destroyed) {
                w.app_handle().exit(0);
            }
            if w.label() == "main"
                && let tauri::WindowEvent::Focused(focused) = ev
                && let Ok(mut t) = MAIN_BLURRED_AT.lock()
            {
                *t = (!focused).then(Instant::now);
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

#[cfg(test)]
mod file_link_tests {
    use super::*;

    #[test]
    fn resolves_mentioned_paths() {
        assert_eq!(resolve_mentioned("/etc/hosts", None), Some("/etc/hosts".into()));
        assert_eq!(resolve_mentioned("re/SPEC.md", Some("/w/proj")), Some("/w/proj/re/SPEC.md".into()));
        assert_eq!(resolve_mentioned("/abs/x.md", Some("/w/proj")), Some("/abs/x.md".into()));
        assert_eq!(resolve_mentioned("re/SPEC.md", None), None);
        assert_eq!(resolve_mentioned("re/SPEC.md", Some("")), None);
        if let Some(home) = crate::code::agents::home_dir() {
            assert_eq!(resolve_mentioned("~/notes.md", None), Some(home.join("notes.md")));
        }
    }

    #[test]
    fn executables_are_revealed_not_opened() {
        let dir = std::env::temp_dir().join(format!("patchbay-file-link-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("Thing.app")).unwrap();
        let doc = dir.join("notes.md");
        let bat = dir.join("run.bat");
        std::fs::write(&doc, "hi").unwrap();
        std::fs::write(&bat, "echo").unwrap();
        let runs = |p: &std::path::Path| runs_when_opened(p, &std::fs::metadata(p).unwrap());
        assert!(!runs(&doc));
        assert!(!runs(&dir));
        assert!(runs(&bat));
        assert!(runs(&dir.join("Thing.app")));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let sh = dir.join("go.sh");
            std::fs::write(&sh, "#!/bin/sh").unwrap();
            std::fs::set_permissions(&sh, std::fs::Permissions::from_mode(0o755)).unwrap();
            assert!(runs(&sh));
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
