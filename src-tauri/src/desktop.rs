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

/// Label of the last-focused normal (non-quick) window.
static LAST_FOCUSED_LABEL: Mutex<Option<String>> = Mutex::new(None);

pub fn record_window_focused(label: &str) {
    if label != QUICK {
        if let Ok(mut last) = LAST_FOCUSED_LABEL.lock() {
            *last = Some(label.to_string());
        }
    }
}

pub fn active_normal_window(app: &AppHandle) -> Option<tauri::WebviewWindow> {
    let last = LAST_FOCUSED_LABEL.lock().ok().and_then(|l| l.clone());
    if let Some(lbl) = last {
        if let Some(w) = app.get_webview_window(&lbl) {
            return Some(w);
        }
    }
    app.get_webview_window("main").or_else(|| {
        app.webview_windows()
            .into_iter()
            .find(|(k, _)| k != QUICK)
            .map(|(_, w)| w)
    })
}

pub fn show_main(app: &AppHandle) {
    if let Some(w) = active_normal_window(app) {
        let _ = w.unminimize();
        if let Some(pos) = MAIN_HIDDEN_AT.lock().ok().and_then(|mut p| p.take())
            && !w.is_visible().unwrap_or(true)
            && !w.is_maximized().unwrap_or(false)
        {
            let _ = w.set_position(pos);
        }
        let _ = w.show();
        let _ = w.set_focus();
        return;
    }
    // No window left: spawn a new one asynchronously.
    let h = app.clone();
    tauri::async_runtime::spawn(async move {
        let _ = new_window(h).await;
    });
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
    let target = active_normal_window(app);
    let Some(w) = target else {
        let h = app.clone();
        tauri::async_runtime::spawn(async move {
            let _ = new_window(h).await;
        });
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
    let app_drawn = title_bar == "app";
    for (label, w) in app.webview_windows() {
        if label == QUICK {
            continue;
        }
        if let Err(e) = w.set_decorations(!app_drawn) {
            tracing::warn!("title bar on {label}: {e:#}");
        }
        // tao's macOS `set_decorations(false)` sets the style mask to Borderless|Resizable and
        // drops Miniaturizable (create-time undecorated windows still include it). Without that
        // mask, NSWindow::miniaturize no-ops — restore it so app-drawn Minimize works.
        if app_drawn {
            if let Err(e) = w.set_minimizable(true) {
                tracing::warn!("title bar on {label}: {e:#}");
            }
        }
        #[cfg(target_os = "linux")]
        {
            let win = w.clone();
            if let Err(e) = w.run_on_main_thread(move || announce_wayland_decorations(&win, app_drawn)) {
                tracing::warn!("title bar on {label}: {e:#}");
            }
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
    let new_win = MenuItem::with_id(app, "new-window", "New window", true, None::<&str>)?;
    let new_chat = MenuItem::with_id(app, "new-chat", "New chat", true, None::<&str>)?;
    let quick = MenuItem::with_id(app, "quick", "Quick entry…", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let sep = PredefinedMenuItem::separator(app)?;
    let menu = Menu::with_items(app, &[&show, &new_win, &new_chat, &quick, &sep, &quit])?;
    let mut builder = TrayIconBuilder::with_id("main")
        .tooltip("Patchbay")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, ev| match ev.id().as_ref() {
            "show" => show_main(app),
            "new-window" => {
                let h = app.clone();
                tauri::async_runtime::spawn(async move {
                    let _ = new_window(h).await;
                });
            }
            "new-chat" => {
                show_main(app);
                if let Some(target) = active_normal_window(app) {
                    let _ = app.emit_to(target.label(), EV_NEW_CHAT, ());
                }
            }
            "quick" => toggle_quick(app),
            "quit" => begin_quit(app, QuitAfter::Exit),
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

static WINDOW_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Open a new desktop window of the app.
#[tauri::command]
pub async fn new_window(app: AppHandle) -> crate::CmdResult<()> {
    let id = format!("win-{}", WINDOW_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
    let mut builder = WebviewWindowBuilder::new(&app, &id, WebviewUrl::App("index.html".into()))
        .title("Patchbay")
        .inner_size(1280.0, 820.0)
        .min_inner_size(760.0, 500.0);

    #[cfg(target_os = "linux")]
    {
        builder = builder.transparent(true);
    }

    let title_bar = app
        .try_state::<crate::AppState>()
        .map(|s| crate::prefs::load(&s.db).title_bar)
        .or_else(|| app.try_state::<crate::daemon_link::DaemonLink>().map(|l| l.startup_prefs().0))
        .unwrap_or_else(|| "app".into());
    let app_drawn = title_bar == "app";
    builder = builder.decorations(!app_drawn);

    let win = builder.build()?;
    #[cfg(target_os = "linux")]
    {
        let w = win.clone();
        let _ = win.run_on_main_thread(move || announce_wayland_decorations(&w, app_drawn));
    }
    let _ = win.show();
    let _ = win.set_focus();
    record_window_focused(&id);
    Ok(())
}

/// From the quick-entry window: hand the text to the main window as a new chat.
#[tauri::command]
fn quick_submit(app: AppHandle, text: String) -> crate::CmdResult<()> {
    hide_quick(&app);
    show_main(&app);
    if let Some(target) = active_normal_window(&app) {
        let _ = app.emit_to(target.label(), EV_QUICK_PROMPT, text);
    }
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
    path_exists_with(&path, cwd.as_deref(), |p| p.exists())
}

/// [`local_path_exists`] with the existence probe handed in. A network location or device path is
/// NEVER probed: `exists` on a UNC path makes Windows connect to it, and this runs for every path any
/// agent's transcript mentions, with no click (text from a web page that an agent echoes is enough).
/// The lexical check comes first, on the text as written and on the resolved path; such a path reads
/// as existing so its link is drawn, and a click asks first (`open_local_path`).
fn path_exists_with(path: &str, cwd: Option<&str>, exists: impl Fn(&std::path::Path) -> bool) -> bool {
    let home = std::path::Path::new(cwd.unwrap_or(""));
    if patchbay_agent::foreign_target(home, std::path::Path::new(path)) {
        return true;
    }
    match resolve_mentioned(path, cwd) {
        Some(p) if patchbay_agent::foreign_target(home, &p) => true,
        Some(p) => exists(&p),
        None => false,
    }
}

/// What a click on a mentioned path does first.
#[derive(Debug, PartialEq)]
enum ClickPlan {
    /// A path on this computer: look at it (it may run) and open it.
    Open(std::path::PathBuf),
    /// Another machine's share or a device path (the text as written): never opened from the app.
    /// Looking at a UNC path makes Windows connect to it and send it the user's credentials, and no
    /// dialog can ask safely (which key an Esc, an Enter or a close presses is the OS's to decide), so
    /// the path is copied for the user to paste into the file manager themselves.
    Copy(String),
}

/// Decide what a click does, from the text of the path alone: no filesystem call. The raw text and
/// the resolved path are both judged, so `\\host` alone (not absolute, so joined to the folder) is
/// still caught.
fn plan_open(path: &str, cwd: Option<&str>) -> Option<ClickPlan> {
    let home = std::path::Path::new(cwd.unwrap_or(""));
    // The raw text first: `\??\UNC\...` is not "absolute" to std, so it may not resolve at all.
    if patchbay_agent::foreign_location(home, std::path::Path::new(path)) {
        return Some(ClickPlan::Copy(path.to_string()));
    }
    let p = resolve_mentioned(path, cwd)?;
    Some(if patchbay_agent::foreign_location(home, &p) { ClickPlan::Copy(path.to_string()) } else { ClickPlan::Open(p) })
}

/// What a click did, for the toast the page shows.
#[derive(Debug, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum ClickDone {
    Opened,
    Copied,
}

/// The two effects a click can have, so a test can see which one ran.
trait ClickOps {
    /// Put the text on the clipboard.
    fn copy(&mut self, text: &str) -> anyhow::Result<()>;
    /// Look at the path (it may run) and open it, or reveal it in the file manager.
    fn open(&mut self, p: &std::path::Path) -> anyhow::Result<()>;
}

/// The click: a network location or device path is copied as written (no filesystem or network
/// call, nothing opened), anything on this computer is opened as before.
fn click(path: &str, cwd: Option<&str>, ops: &mut impl ClickOps) -> anyhow::Result<ClickDone> {
    match plan_open(path, cwd).ok_or_else(|| anyhow!("{path} is relative and has no folder"))? {
        ClickPlan::Open(p) => {
            ops.open(&p)?;
            Ok(ClickDone::Opened)
        }
        ClickPlan::Copy(text) => {
            ops.copy(&text)?;
            Ok(ClickDone::Copied)
        }
    }
}

struct RealClick<'a>(&'a tauri::AppHandle);

impl ClickOps for RealClick<'_> {
    fn copy(&mut self, text: &str) -> anyhow::Result<()> {
        use tauri_plugin_clipboard_manager::ClipboardExt;
        self.0.clipboard().write_text(text.to_string())?;
        Ok(())
    }

    fn open(&mut self, p: &std::path::Path) -> anyhow::Result<()> {
        let meta = std::fs::metadata(p).map_err(|e| anyhow!("{}: {e}", p.display()))?;
        if runs_when_opened(p, &meta) {
            tauri_plugin_opener::reveal_item_in_dir(p)?;
        } else {
            tauri_plugin_opener::open_path(p, None::<&str>)?;
        }
        Ok(())
    }
}

/// Open a mentioned file or folder in the OS's default app. Anything that would run is shown in
/// the file manager instead. A network location or device path is copied to the clipboard instead
/// (`Copied`), never opened.
#[tauri::command]
async fn open_local_path(app: tauri::AppHandle, path: String, cwd: Option<String>) -> crate::CmdResult<ClickDone> {
    Ok(click(&path, cwd.as_deref(), &mut RealClick(&app))?)
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
    // pb-20261111-22: the updater restart goes through the same wait offer — the gate calls
    // request_restart only when the list is empty (or Quit now was chosen).
    begin_quit(&app, QuitAfter::Restart);
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
/// What a finished quit does once the wait list empties (pb-20261111-22).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum QuitAfter {
    Exit,
    Restart,
}

/// The one gate every quit path funnels through: the pending action, whether the wait dialog
/// is on screen, and a latch so our own exit/restart never re-enters the gate.
#[derive(Default)]
pub struct QuitFlow {
    inner: Mutex<QuitFlowInner>,
}

#[derive(Default)]
struct QuitFlowInner {
    after: Option<QuitAfter>,
    showing: bool,
    performing: bool,
}

/// The decision every quit path makes. Attached (no AppState of its own) quits at once — the
/// daemon keeps working, there is nothing to wait for (no false wait). The standalone owner
/// offers the wait dialog while turns are actually running; with nothing running there is
/// nothing to wait for either. A quit already under way is ignored; a quit while the dialog
/// is already up RE-EMITS (re-raised window, fresh event) instead of being swallowed, so a
/// second Quit can never be a dead button.
#[derive(PartialEq, Eq, Debug)]
enum QuitPlan {
    GoNow,
    ShowWait,
    Ignore,
}

fn plan_quit(owns_state: bool, active_turns: usize, performing: bool) -> QuitPlan {
    if performing {
        QuitPlan::Ignore
    } else if owns_state && active_turns > 0 {
        QuitPlan::ShowWait
    } else {
        QuitPlan::GoNow
    }
}

/// Whether a destroyed window may reset the quit gate: ONLY when no normal window remains
/// (the dialog has nowhere to live). Closing a second window while the dialog is up in the
/// first must not silently drop the pending quit (pb-20261111-22 round 3) — with a window
/// left, the dialog continues there (every window polls `quit_wait_status`) or the next Quit
/// re-raises it.
fn destroyed_resets_gate(remaining_normal: usize) -> bool {
    remaining_normal == 0
}

/// What `quit_wait_status` reports as "showing": a wait only counts while a quit is actually
/// pending — a status response that raced a Keep-open cancel (showing, no `after`) must not
/// re-open the dialog and proceed into an exit (pb-20261111-22 round 3).
fn quit_status_pending(showing: bool, after: Option<QuitAfter>) -> bool {
    showing && after.is_some()
}

/// Every quit path calls this: tray Quit, the last window's close, Cmd-Q / the app menu
/// (ExitRequested), and the updater's restart (gated before `request_restart`).
fn begin_quit(app: &AppHandle, after: QuitAfter) {
    let state = app.try_state::<crate::AppState>();
    let active = state.as_ref().map(|s| s.code.busy_sessions().len()).unwrap_or(0);
    let flow = app.state::<QuitFlow>();
    let plan = {
        let g = flow.inner.lock().unwrap_or_else(|e| e.into_inner());
        plan_quit(state.is_some(), active, g.performing)
    };
    match plan {
        QuitPlan::Ignore => {}
        QuitPlan::GoNow => perform_quit(app, after),
        QuitPlan::ShowWait => {
            if active_normal_window(app).is_some() {
                {
                    let mut g = flow.inner.lock().unwrap_or_else(|e| e.into_inner());
                    g.after = Some(after);
                    g.showing = true;
                }
                // Raise/unminimize/show/focus FIRST (the house routine: it also restores a
                // hidden-to-tray window and spawns one if none is left), so a hidden, minimized
                // or other-Space window can never make Quit look dead while showing sticks.
                show_main(app);
                if let Some(w) = active_normal_window(app) {
                    let _ = w.emit("quit://wait", ());
                }
            } else {
                // No window to ask in: nothing can show the dialog, so go now.
                perform_quit(app, after);
            }
        }
    }
}

/// Carry out a decided quit: latch first (our own ExitRequested must not re-enter the gate),
/// then exit or restart. `RunEvent::Exit` still runs `state.shutdown()` on the way out, so a
/// clean wait leaves no flags and Quit now leaves them set (the shutdown guard).
fn finish_quit(app: &AppHandle, after: QuitAfter) {
    match after {
        QuitAfter::Exit => app.exit(0),
        QuitAfter::Restart => app.request_restart(),
    }
}

fn perform_quit(app: &AppHandle, after: QuitAfter) {
    {
        let flow = app.state::<QuitFlow>();
        let mut g = flow.inner.lock().unwrap_or_else(|e| e.into_inner());
        if g.performing {
            return;
        }
        g.performing = true;
        g.showing = false;
        g.after = None;
    }
    let Some(state) = app.try_state::<crate::AppState>().map(|s| s.inner().clone()) else {
        // Attached: no AppState here — the daemon owns its queues, nothing of ours to tell.
        finish_quit(app, after);
        return;
    };
    // Tell the senders of queued peer messages BEFORE the loops die with the app, bounded
    // (pb-20261107-30): the drains are FIFO ahead of the stop_all that shutdown() runs after
    // this task's finish, so a notice-woken sender is covered (no orphans) and a hung
    // notice can never keep the quit from happening.
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        crate::tell_queued_senders_then(&state, move || finish_quit(&app, after)).await;
    });
}

/// The wait dialog's "Quit now" (and its auto-quit when the list empties): carry out the
/// pending quit. Quit now is today's stop_all behaviour — flags stay set, the relaunch offers
/// those sessions (the shutdown guard keeps stop_all's death throes from clearing them).
#[tauri::command]
fn quit_proceed(app: AppHandle) {
    let flow = app.state::<QuitFlow>();
    let after = flow.inner.lock().unwrap_or_else(|e| e.into_inner()).after;
    // No pending quit (a Keep-open cancel won a race with an in-flight status response):
    // proceeding must be a no-op, never an exit the user cancelled (pb-20261111-22 round 3).
    let Some(after) = after else { return };
    perform_quit(&app, after);
}

/// What the wait dialog needs to (re)draw itself after a reload, a second window, or a poll:
/// whether a quit is waiting, and the sessions still working FROM THE BACKEND'S OWN VIEW —
/// the frontend must not guess the list (a stale store could auto-quit an empty one).
#[tauri::command]
fn quit_wait_status(app: AppHandle) -> serde_json::Value {
    let (showing, after) = {
        let flow = app.state::<QuitFlow>();
        let g = flow.inner.lock().unwrap_or_else(|e| e.into_inner());
        (g.showing, g.after)
    };
    // A wait only counts while a quit is pending (see quit_status_pending): a stale response
    // from before a Keep-open cancel must not re-open the dialog. And with no wait pending
    // there is nothing to list — no busy_sessions walk at all (poll cost).
    let showing = quit_status_pending(showing, after);
    if !showing {
        return serde_json::json!({ "showing": false, "active": [] });
    }
    let active: Vec<serde_json::Value> = app
        .try_state::<crate::AppState>()
        .map(|s| {
            s.code
                .busy_sessions()
                .into_iter()
                .map(|id| {
                    let title = s
                        .db
                        .get_code_session(&id)
                        .ok()
                        .flatten()
                        .map(|c| c.title)
                        .unwrap_or_else(|| id.clone());
                    let waiting = crate::waiting_on_person(&s, &id);
                    serde_json::json!({ "id": id, "title": title, "waiting": waiting })
                })
                .collect()
        })
        .unwrap_or_default();
    serde_json::json!({ "showing": showing, "active": active })
}

/// The wait dialog's "Keep Patchbay open": drop the pending quit; no flag is touched.
#[tauri::command]
fn quit_cancel(app: AppHandle) {
    {
        let flow = app.state::<QuitFlow>();
        let mut g = flow.inner.lock().unwrap_or_else(|e| e.into_inner());
        if g.performing {
            return;
        }
        g.showing = false;
        g.after = None;
    }
    // Anything the quit flow parked while it was up (a pending update's installing flag)
    // resets here: cancelling must leave every button retryable (pb-20261111-22).
    let _ = app.emit("quit://cancelled", ());
}

fn invoke_handler() -> impl Fn(tauri::ipc::Invoke) -> bool + Send + Sync + 'static {
    let desktop_only: Box<dyn Fn(tauri::ipc::Invoke) -> bool + Send + Sync> =
        Box::new(tauri::generate_handler![
            new_window,
            quick_submit,
            quick_dismiss,
            write_text_file,
            local_path_exists,
            open_local_path,
            update_check,
            update_install,
            quit_proceed,
            quit_cancel,
            quit_wait_status,
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
    crate::fd_limit::raise_at_startup();

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_clipboard_manager::init())
        .manage(PendingUpdate::default())
        .manage(QuitFlow::default())
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
                        if let Some(target) = active_normal_window(&h) {
                            let _ = h.emit_to(target.label(), EV_NOTIFICATION_OPEN, open.clone());
                        }
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
            // The last normal window asks the quit gate instead of dying at once: a wait
            // dialog while turns run, an immediate quit otherwise (pb-20261111-22). Other
            // windows still close freely; a quit already under way lets closes through.
            if w.label() != QUICK && matches!(ev, tauri::WindowEvent::CloseRequested { .. }) {
                let app = w.app_handle();
                let others = app
                    .webview_windows()
                    .into_iter()
                    .filter(|(label, _)| *label != QUICK && *label != w.label())
                    .count();
                let performing = app
                    .state::<QuitFlow>()
                    .inner
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .performing;
                if others == 0 && !performing {
                    if let tauri::WindowEvent::CloseRequested { api, .. } = ev {
                        api.prevent_close();
                    }
                    begin_quit(app, QuitAfter::Exit);
                }
            }
            // The hidden quick-entry window must not keep the app alive after normal windows close.
            if w.label() != QUICK && matches!(ev, tauri::WindowEvent::Destroyed) {
                let remaining = w
                    .app_handle()
                    .webview_windows()
                    .into_iter()
                    .filter(|(label, _)| label != QUICK && label != w.label())
                    .count();
                // Reset the gate ONLY when no normal window remains (pb-20261111-22 round 3):
                // closing a second window while the dialog lives in the first must not drop
                // the pending quit. With a window left the dialog continues there, and the
                // next Quit re-raises it anyway.
                if destroyed_resets_gate(remaining) {
                    if let Some(flow) = w.app_handle().try_state::<QuitFlow>() {
                        let mut g = flow.inner.lock().unwrap_or_else(|e| e.into_inner());
                        g.showing = false;
                    }
                }
                if remaining == 0 {
                    w.app_handle().exit(0);
                }
            }
            if w.label() != QUICK
                && let tauri::WindowEvent::Focused(focused) = ev
            {
                if *focused {
                    record_window_focused(w.label());
                }
                if let Ok(mut t) = MAIN_BLURRED_AT.lock() {
                    *t = (!focused).then(Instant::now);
                }
            }
        })
        .invoke_handler(invoke_handler())
        .build(tauri::generate_context!())
        .expect("error while building Patchbay")
        .run(|app, event| match event {
            // Cmd-Q, the app menu's Quit, and our own exit/restart all pass through here.
            // Attached (no AppState): let it quit — the daemon keeps working, nothing to wait
            // for. Standalone: offer the wait dialog first (pb-20261111-22). Our own performed
            // quit/restart passes straight through (the latch above).
            tauri::RunEvent::ExitRequested { api, .. } => {
                let performing = app
                    .state::<QuitFlow>()
                    .inner
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .performing;
                if !performing && app.try_state::<crate::AppState>().is_some() {
                    api.prevent_exit();
                    begin_quit(app, QuitAfter::Exit);
                }
            }
            tauri::RunEvent::Exit => {
                if let Some(state) = app.try_state::<crate::AppState>() {
                    state.shutdown();
                }
            }
            _ => {}
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

    /// A network location in an agent's text is not looked at to draw its link, and a click on it
    /// does not open it: it is copied (the text as written), with no filesystem call and nothing
    /// handed to the opener. All three effects are mocked here. Decided on the text, so nothing can
    /// connect (203.0.113.1 is a documentation address nobody answers: a connection attempt would
    /// take far more than a second). Windows only for the foreign half; type-checked everywhere.
    #[test]
    fn a_network_location_is_copied_never_opened_or_looked_at() {
        #[derive(Default)]
        struct Mock {
            copied: Vec<String>,
            opened: Vec<std::path::PathBuf>,
        }
        impl ClickOps for Mock {
            fn copy(&mut self, text: &str) -> anyhow::Result<()> {
                self.copied.push(text.to_string());
                Ok(())
            }
            fn open(&mut self, p: &std::path::Path) -> anyhow::Result<()> {
                self.opened.push(p.to_path_buf());
                Ok(())
            }
        }
        if cfg!(windows) {
            let started = std::time::Instant::now();
            for spelling in [
                r"\\203.0.113.1\s\x.txt",
                "//203.0.113.1/s/x.txt",
                r"\/203.0.113.1/s/x",
                r"\??\UNC\203.0.113.1\s\x",
                r"\\?\UNC\203.0.113.1\s\x",
                r"\\.\UNC\203.0.113.1\s\x",
                r"\\.\PhysicalDrive0",
                r"\\203.0.113.1",
                r"\\localhost\C$\Users\x",
            ] {
                for cwd in [None, Some(r"C:\Users\x\proj")] {
                    let mut ops = Mock::default();
                    assert_eq!(click(spelling, cwd, &mut ops).unwrap(), ClickDone::Copied, "{spelling} was not copied");
                    assert_eq!(ops.copied, vec![spelling.to_string()], "the clipboard gets the text as written: {spelling}");
                    assert!(ops.opened.is_empty(), "{spelling} was handed to the opener");
                }
                // Rendering: the existence probe is mocked and must NEVER be called for any spelling.
                let probes = std::cell::Cell::new(0);
                for cwd in [None, Some(r"C:\Users\x\proj")] {
                    assert!(path_exists_with(spelling, cwd, |_| { probes.set(probes.get() + 1); false }), "the link is drawn: {spelling}");
                }
                assert_eq!(probes.get(), 0, "rendering {spelling} probed the filesystem");
            }
            assert!(started.elapsed() < std::time::Duration::from_secs(1), "deciding took {:?}: a network attempt", started.elapsed());
        }
        // Everywhere: a path on this computer is probed and found as before (the control for the 0 above).
        let probes = std::cell::Cell::new(0);
        let here = std::env::temp_dir().join("a-local-file.txt");
        assert!(!path_exists_with(&here.display().to_string(), None, |_| { probes.set(probes.get() + 1); false }));
        assert_eq!(probes.get(), 1, "a local path is probed");
        // Everywhere: a path on this computer is opened as before, and nothing is copied.
        let mut ops = Mock::default();
        assert_eq!(click(&here.display().to_string(), None, &mut ops).unwrap(), ClickDone::Opened);
        assert_eq!(ops.opened, vec![here.clone()]);
        assert!(ops.copied.is_empty());
        // A relative path inside the folder opens as before too.
        let mut ops = Mock::default();
        let dir = std::env::temp_dir();
        assert_eq!(click("sub/a.txt", Some(&dir.display().to_string()), &mut ops).unwrap(), ClickDone::Opened);
        assert_eq!(ops.opened, vec![dir.join("sub/a.txt")]);
        assert!(ops.copied.is_empty());
    }

    /// A mentioned path that goes through a link in the folder to another machine's share is drawn as a
    /// link without a probe: `exists` follows the link and connects to the share, for every path a
    /// transcript mentions, with no click. The probe handed in is the real `exists`, counted.
    #[test]
    #[cfg(windows)]
    fn a_mention_behind_a_link_to_a_share_is_drawn_without_a_probe() {
        let dir = std::env::temp_dir().join(format!("pb-mention-{}", crate::db::new_id()));
        // `PB_E2E_LINK`: a link made earlier by whoever watches the network, so the OS's own reaction to
        // creating a link to a share is over before the call under test (its folder is then the project).
        let pre = std::env::var("PB_E2E_LINK").ok().map(std::path::PathBuf::from);
        let proj = pre.as_ref().and_then(|l| l.parent().map(|d| d.to_path_buf())).unwrap_or_else(|| dir.join("proj"));
        std::fs::create_dir_all(&proj).unwrap();
        let link = pre.clone().unwrap_or_else(|| proj.join("to_share"));
        if pre.is_none() {
            let share = std::env::var("PB_E2E_SHARE").unwrap_or_else(|_| r"\\192.0.2.1\share".into());
            if let Err(e) = std::os::windows::fs::symlink_dir(&share, &link) {
                eprintln!("skipped: cannot create a directory symlink here ({e})");
                let _ = std::fs::remove_dir_all(&dir);
                return;
            }
        }
        let cwd = proj.to_string_lossy().into_owned();
        let probes = std::cell::Cell::new(0);
        let started = std::time::Instant::now();
        for mention in ["to_share/a.png".to_owned(), r"to_share\deep\a.png".to_owned(), link.join("a.png").to_string_lossy().into_owned()] {
            assert!(path_exists_with(&mention, Some(&cwd), |p| { probes.set(probes.get() + 1); p.exists() }), "the link is drawn: {mention}");
        }
        assert_eq!(probes.get(), 0, "a mention behind a link to a share was probed");
        assert!(started.elapsed() < std::time::Duration::from_secs(1), "deciding took {:?}: a network attempt", started.elapsed());
        if pre.is_none() {
            let _ = std::fs::remove_dir(&link);
            let _ = std::fs::remove_dir_all(&dir);
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

/// pb-20261111-22: the quit gate's decision table — attached quits at once (no dialog: the
/// daemon keeps working), the standalone owner waits only while turns run, re-entry ignored.
#[cfg(all(test, feature = "ipc-test"))]
#[test]
fn quit_gate_decides_wait_vs_go_now() {
    assert_eq!(plan_quit(false, 3, false), QuitPlan::GoNow, "attached with turns: no dialog");
    assert_eq!(plan_quit(false, 0, false), QuitPlan::GoNow, "attached idle");
    assert_eq!(plan_quit(true, 2, false), QuitPlan::ShowWait, "standalone with turns: wait");
    assert_eq!(plan_quit(true, 0, false), QuitPlan::GoNow, "standalone idle: nothing to wait for");
    // A second Quit while the dialog is up is NOT ignored: it re-raises and re-emits (the
    // plan no longer looks at `showing`), so the quit button can never be a dead one.
    assert_eq!(plan_quit(true, 2, false), QuitPlan::ShowWait, "second quit re-emits");
    assert_eq!(plan_quit(true, 2, true), QuitPlan::Ignore, "already quitting");
    // Destroyed-window reset (round 3): only with no normal window left — closing a second
    // window while the dialog lives in the first must not drop the pending quit.
    assert!(destroyed_resets_gate(0), "no window left: the gate may reset");
    assert!(!destroyed_resets_gate(1), "a window remains: the pending quit stays");
    // Cancel-race status (round 3): showing without a pending quit reports nothing.
    assert!(quit_status_pending(true, Some(QuitAfter::Exit)));
    assert!(!quit_status_pending(true, None), "a cancelled wait must not report showing");
    assert!(!quit_status_pending(false, Some(QuitAfter::Exit)));
}
