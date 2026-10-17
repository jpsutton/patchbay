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
