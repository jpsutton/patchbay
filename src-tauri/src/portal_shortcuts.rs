//! The quick-entry shortcut on Wayland sessions, through the xdg-desktop-portal GlobalShortcuts
//! interface.
//!
//! The global-shortcut plugin grabs keys over X11. A Wayland compositor only forwards keys to
//! XWayland while an X11 window has focus, so on a Wayland session the plugin's shortcut fires
//! only while Patchbay itself is in front. The portal asks the compositor to watch for the
//! shortcut instead. The desktop decides the final key combination: the first bind usually asks
//! the user to confirm it, and the user can change it later in the desktop's settings, so the
//! accelerator from the preferences is only a preferred trigger.
//!
//! When the portal is missing or fails to start, the plugin takes over.

use std::sync::Mutex;

use ashpd::desktop::Session;
use ashpd::desktop::global_shortcuts::{GlobalShortcuts, NewShortcut};
use futures::StreamExt;
use tauri::AppHandle;
use tauri_plugin_global_shortcut::{Code, Modifiers, Shortcut};
use tokio::sync::watch;

/// The id of our one shortcut. Desktops store the user's chosen trigger under it.
const QUICK_ENTRY: &str = "quick-entry";

/// The running portal task's accelerator channel, while the portal handles the shortcut.
static PORTAL: Mutex<Option<watch::Sender<String>>> = Mutex::new(None);

/// Whether this is a Wayland session, whatever backend GTK picked for our windows.
pub fn wayland_session() -> bool {
    let set = |k: &str| std::env::var_os(k).is_some_and(|v| !v.is_empty());
    set("WAYLAND_DISPLAY") || std::env::var("XDG_SESSION_TYPE").is_ok_and(|t| t == "wayland")
}

/// Start handling the quick-entry shortcut through the portal, beginning with `accel`. When the
/// portal turns out to be unavailable, `fallback` registers the then-current accelerator with
/// the plugin.
pub fn start(app: AppHandle, accel: &str, fallback: fn(&AppHandle, &str)) {
    let (tx, rx) = watch::channel(accel.to_string());
    if let Ok(mut p) = PORTAL.lock() {
        *p = Some(tx);
    }
    tauri::async_runtime::spawn(async move {
        if let Err(e) = run(&app, rx).await {
            tracing::warn!("global shortcuts portal unavailable, using X11 key grab: {e:#}");
            // Take the channel under the lock so a concurrent preference change lands either in
            // the channel (seen here) or in the plugin, never in neither.
            let Ok(mut p) = PORTAL.lock() else { return };
            if let Some(tx) = p.take() {
                fallback(&app, &tx.borrow());
            }
        }
    });
}

/// Hand a new accelerator to the portal task. False when the portal is not in use, so the
/// caller registers it with the plugin instead.
pub fn update(accel: &str) -> bool {
    match PORTAL.lock().ok().as_ref().and_then(|p| p.as_ref()) {
        Some(tx) => {
            tx.send_replace(accel.to_string());
            true
        }
        None => false,
    }
}

/// Bind the shortcut, rebind it whenever the accelerator changes, and open the quick-entry
/// window when the compositor reports it. Returns an error only when the portal cannot be used
/// at all.
async fn run(app: &AppHandle, mut rx: watch::Receiver<String>) -> anyhow::Result<()> {
    let portal = GlobalShortcuts::new().await?;
    let mut activated = portal.receive_activated().await?;
    // A session proves the portal works. Later bind failures come from user choices (a cancelled
    // confirmation dialog), which must not switch to the X11 grab.
    portal.create_session().await?.close().await?;
    let mut session: Option<Session<'_, GlobalShortcuts<'_>>> = None;
    loop {
        let accel = rx.borrow_and_update().trim().to_string();
        if let Some(s) = session.take() {
            let _ = s.close().await;
        }
        if !accel.is_empty() {
            match bind(&portal, &accel).await {
                Ok(s) => session = Some(s),
                Err(e) => tracing::warn!("quick entry shortcut {accel}: {e:#}"),
            }
        }
        loop {
            tokio::select! {
                changed = rx.changed() => {
                    if changed.is_err() {
                        return Ok(());
                    }
                    break;
                }
                ev = activated.next() => match ev {
                    Some(ev) if ev.shortcut_id() == QUICK_ENTRY && session.is_some() => {
                        crate::desktop::toggle_quick(app);
                    }
                    Some(_) => {}
                    None => anyhow::bail!("portal stopped sending shortcut events"),
                },
            }
        }
    }
}

async fn bind<'a>(portal: &GlobalShortcuts<'a>, accel: &str) -> anyhow::Result<Session<'a, GlobalShortcuts<'a>>> {
    let session = portal.create_session().await?;
    let trigger = accel.parse::<Shortcut>().ok().and_then(|s| xdg_trigger(&s));
    let shortcut = NewShortcut::new(QUICK_ENTRY, "Open the Patchbay quick entry prompt")
        .preferred_trigger(trigger.as_deref());
    let bound = portal.bind_shortcuts(&session, &[shortcut], None).await?.response();
    match bound {
        Ok(b) => {
            let got = b.shortcuts().iter().find(|s| s.id() == QUICK_ENTRY);
            tracing::info!(
                "quick entry shortcut bound through the portal: {}",
                got.map_or("(none)", |s| s.trigger_description())
            );
            Ok(session)
        }
        Err(e) => {
            let _ = session.close().await;
            Err(e.into())
        }
    }
}

/// A Tauri accelerator as an XDG shortcuts-spec trigger, e.g. `CTRL+SHIFT+space`. None when the
/// key has no known keysym name; the desktop then asks the user for one.
fn xdg_trigger(s: &Shortcut) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    for (m, name) in [
        (Modifiers::CONTROL, "CTRL"),
        (Modifiers::ALT, "ALT"),
        (Modifiers::SHIFT, "SHIFT"),
        (Modifiers::SUPER, "LOGO"),
    ] {
        if s.mods.contains(m) {
            parts.push(name.into());
        }
    }
    if s.mods.contains(Modifiers::META) && !s.mods.contains(Modifiers::SUPER) {
        parts.push("LOGO".into());
    }
    parts.push(keysym(s.key)?);
    Some(parts.join("+"))
}

/// The xkb keysym name of a key.
fn keysym(code: Code) -> Option<String> {
    let name = code.to_string();
    if let Some(letter) = name.strip_prefix("Key") {
        return Some(letter.to_lowercase());
    }
    if let Some(digit) = name.strip_prefix("Digit") {
        return Some(digit.into());
    }
    if name.len() <= 3 && name.starts_with('F') && name[1..].parse::<u8>().is_ok() {
        return Some(name);
    }
    let sym = match code {
        Code::Space => "space",
        Code::Enter => "Return",
        Code::Tab => "Tab",
        Code::Escape => "Escape",
        Code::Backspace => "BackSpace",
        Code::Delete => "Delete",
        Code::Insert => "Insert",
        Code::Home => "Home",
        Code::End => "End",
        Code::PageUp => "Page_Up",
        Code::PageDown => "Page_Down",
        Code::ArrowUp => "Up",
        Code::ArrowDown => "Down",
        Code::ArrowLeft => "Left",
        Code::ArrowRight => "Right",
        Code::Minus => "minus",
        Code::Equal => "equal",
        Code::Comma => "comma",
        Code::Period => "period",
        Code::Slash => "slash",
        Code::Backslash => "backslash",
        Code::Semicolon => "semicolon",
        Code::Quote => "apostrophe",
        Code::Backquote => "grave",
        Code::BracketLeft => "bracketleft",
        Code::BracketRight => "bracketright",
        Code::PrintScreen => "Print",
        _ => return None,
    };
    Some(sym.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trigger(accel: &str) -> Option<String> {
        xdg_trigger(&accel.parse::<Shortcut>().unwrap())
    }

    #[test]
    fn accelerators_become_xdg_triggers() {
        assert_eq!(trigger("Alt+Space").as_deref(), Some("ALT+space"));
        assert_eq!(trigger("CommandOrControl+Shift+Space").as_deref(), Some("CTRL+SHIFT+space"));
        assert_eq!(trigger("Super+K").as_deref(), Some("LOGO+k"));
        assert_eq!(trigger("Ctrl+Alt+7").as_deref(), Some("CTRL+ALT+7"));
        assert_eq!(trigger("Shift+F12").as_deref(), Some("SHIFT+F12"));
        assert_eq!(trigger("Ctrl+PageDown").as_deref(), Some("CTRL+Page_Down"));
    }

    #[test]
    fn unknown_keys_leave_the_trigger_to_the_desktop() {
        assert_eq!(trigger("Ctrl+NumpadAdd"), None);
    }
}
