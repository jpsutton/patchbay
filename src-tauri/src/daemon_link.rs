//! The desktop app on a running patchbayd's backend (see ipc.rs). When the daemon owns the data
//! folder, the app attaches to it at startup, forwards the window's shared commands to it, keeps
//! the desktop-only parts of the preferences (quick-entry shortcut, title bar) working here, and
//! reconnects when the daemon restarts.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tauri::{AppHandle, Emitter, Manager};

use crate::api::ChanFactory;
use crate::ipc::client::{AttachError, Conn, EventSink, LOST};

/// Window events: the connection to the daemon dropped, and came back (the windows reload then).
pub const EV_DISCONNECTED: &str = "daemon://disconnected";
pub const EV_CONNECTED: &str = "daemon://connected";

/// How long startup waits for a daemon that holds the folder but has no socket up yet.
const STARTUP_WAIT: Duration = Duration::from_secs(3);
const RECONNECT_EVERY: Duration = Duration::from_secs(2);

pub struct DaemonLink {
    app: AppHandle,
    data_dir: PathBuf,
    conn: Mutex<Option<Arc<Conn>>>,
}

impl DaemonLink {
    /// Attach to the daemon that holds `data_dir`. Blocks while it waits, so call it at startup.
    pub fn attach(app: &AppHandle, data_dir: PathBuf) -> Result<Arc<DaemonLink>, AttachError> {
        let link = Arc::new(DaemonLink { app: app.clone(), data_dir, conn: Mutex::new(None) });
        let l = link.clone();
        let conn = tauri::async_runtime::block_on(async move {
            let deadline = Instant::now() + STARTUP_WAIT;
            loop {
                match l.open().await {
                    Err(AttachError::Unreachable(_)) if Instant::now() < deadline => {
                        tokio::time::sleep(Duration::from_millis(250)).await;
                    }
                    r => return r,
                }
            }
        })?;
        link.set_conn(Some(conn));
        Ok(link)
    }

    async fn open(self: &Arc<Self>) -> Result<Arc<Conn>, AttachError> {
        let app = self.app.clone();
        let on_event: EventSink = Arc::new(move |name: &str, payload: Value| {
            let _ = app.emit(name, payload);
        });
        let weak = Arc::downgrade(self);
        Conn::open(&self.data_dir, on_event, move || {
            if let Some(link) = weak.upgrade() {
                link.lost();
            }
        })
        .await
    }

    fn set_conn(&self, conn: Option<Arc<Conn>>) {
        *self.conn.lock().unwrap_or_else(|e| e.into_inner()) = conn;
    }

    /// The connection dropped: tell the windows, then try again until the daemon is back. The app
    /// never takes the folder over itself, since the daemon may only be restarting.
    fn lost(self: Arc<Self>) {
        self.set_conn(None);
        tracing::warn!("{LOST}; reconnecting");
        let _ = self.app.emit(EV_DISCONNECTED, ());
        tauri::async_runtime::spawn(async move {
            loop {
                tokio::time::sleep(RECONNECT_EVERY).await;
                match self.open().await {
                    Ok(conn) => {
                        self.set_conn(Some(conn));
                        tracing::info!("reconnected to patchbayd");
                        let _ = self.app.emit(EV_CONNECTED, ());
                        // Streams and channels didn't survive: start the windows afresh.
                        for w in self.app.webview_windows().values() {
                            let _ = w.eval("location.reload()");
                        }
                        return;
                    }
                    Err(e @ AttachError::Version { .. }) => {
                        use tauri_plugin_dialog::{DialogExt, MessageDialogKind};
                        let app = self.app.clone();
                        self.app
                            .dialog()
                            .message(e.to_string())
                            .title("Patchbay can't continue")
                            .kind(MessageDialogKind::Error)
                            .show(move |_| app.exit(1));
                        return;
                    }
                    Err(_) => {}
                }
            }
        });
    }

    /// Run a shared command on the daemon.
    pub async fn call(&self, cmd: &str, args: Value, chans: &ChanFactory) -> Result<Value, String> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner()).clone();
        match conn {
            Some(c) => c.call(cmd, args, chans).await,
            None => Err(LOST.to_owned()),
        }
    }

    /// The saved title bar and quick-entry shortcut, to apply at startup.
    pub fn startup_prefs(&self) -> (String, String) {
        let none: ChanFactory = Arc::new(|_| None);
        let p = tauri::async_runtime::block_on(self.call("get_preferences", json!({}), &none)).unwrap_or_else(|e| {
            tracing::warn!("preferences from patchbayd: {e}");
            Value::Null
        });
        (
            p["titleBar"].as_str().unwrap_or("system").to_owned(),
            p["quickEntryShortcut"].as_str().unwrap_or_default().to_owned(),
        )
    }

    /// `set_preferences` through the daemon. The daemon saves them, but its `DesktopHooks` are
    /// missing, so the shortcut and title bar apply here, in the order `crate::set_preferences`
    /// uses: the new shortcut is registered first and put back if the save fails.
    pub async fn set_preferences(&self, args: Value, chans: &ChanFactory) -> Result<Value, String> {
        let shortcut = args["prefs"]["quickEntryShortcut"].as_str().unwrap_or_default().to_owned();
        let title_bar = args["prefs"]["titleBar"].as_str().unwrap_or("system").to_owned();
        crate::desktop::validate_shortcut(&shortcut)?;
        let old = self.call("get_preferences", json!({}), chans).await?;
        let old_shortcut = old["quickEntryShortcut"].as_str().unwrap_or_default().to_owned();
        let shortcut_changed = old_shortcut.trim() != shortcut.trim();
        if shortcut_changed && let Err(e) = crate::desktop::set_quick_shortcut(&self.app, &shortcut) {
            let _ = crate::desktop::set_quick_shortcut(&self.app, &old_shortcut);
            return Err(format!("{e:#}"));
        }
        let saved = self.call("set_preferences", args, chans).await;
        if saved.is_err() {
            if shortcut_changed {
                let _ = crate::desktop::set_quick_shortcut(&self.app, &old_shortcut);
            }
            return saved;
        }
        if old["titleBar"].as_str().unwrap_or("system") != title_bar {
            crate::desktop::apply_title_bar(&self.app, &title_bar);
        }
        saved
    }
}

/// Whether this process runs on a daemon's backend.
pub fn attached<R: tauri::Runtime>(m: &impl Manager<R>) -> Option<Arc<DaemonLink>> {
    m.try_state::<Arc<DaemonLink>>().map(|s| s.inner().clone())
}
