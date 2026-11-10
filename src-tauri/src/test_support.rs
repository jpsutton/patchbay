//! What the tests need from the machine they run on.

use std::path::PathBuf;

/// A fresh folder under the temp dir whose path leaves room for a Unix socket inside it: macOS
/// puts `temp_dir()` under `/var/folders/…` (about 50 characters) and caps a socket path at
/// 104, which a full uuid in the name overruns.
pub fn short_dir(tag: &str) -> PathBuf {
    let id = crate::db::new_id();
    std::env::temp_dir().join(format!("pb-{tag}-{}", &id[..8]))
}

/// Serialize the tests that set `GEMINI_HOME`. `std::env::set_var` (edition 2024: `unsafe`)
/// is process-global: two tests swapping it at once would read each other's directories —
/// one could even clear `auth` from the other's settings file mid-test. Hold this for the
/// whole test, including the `remove_var` at the end.
pub fn gemini_home_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    // A poisoned lock still serializes: the previous holder's panic already failed its test.
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Raise this process's open-file limit as the app does at startup (`fd_limit`). A shell on
/// macOS starts at 256, too few for the mesh tests, which run several app instances with their
/// runtimes, databases and sockets in one process while cargo runs tests in parallel. Once per
/// process.
pub fn raise_fd_limit() {
    #[cfg(unix)]
    {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            let _ = crate::fd_limit::raise();
        });
    }
}

/// One scratch HOME for every child process a test spawns (shells, agent CLIs, version probes).
/// Children write their own state under `$HOME` (`~/.config/opencode`, `~/.cursor`, shell history,
/// ...), which must never land in the developer's real home. Created once per test process.
pub fn sandbox_home() -> &'static std::path::Path {
    static HOME: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    HOME.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("pb-test-home-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        dir
    })
}

/// The variables a child resolves its state directories from, pointed into `sandbox_home()`,
/// and the ambient account directory variable dropped (a developer shell may export one).
fn sandbox_env() -> Vec<(&'static str, PathBuf)> {
    let h = sandbox_home();
    vec![
        ("HOME", h.to_path_buf()),
        ("XDG_CONFIG_HOME", h.join(".config")),
        ("XDG_DATA_HOME", h.join(".local/share")),
        ("XDG_STATE_HOME", h.join(".local/state")),
        ("XDG_CACHE_HOME", h.join(".cache")),
    ]
}

const SANDBOX_REMOVED: [&str; 2] = ["CLAUDE_CONFIG_DIR", "GROK_HOME"];

pub fn isolate_std(cmd: &mut std::process::Command) {
    for (k, v) in sandbox_env() {
        cmd.env(k, v);
    }
    for k in SANDBOX_REMOVED {
        cmd.env_remove(k);
    }
}

pub fn isolate_tokio(cmd: &mut tokio::process::Command) {
    for (k, v) in sandbox_env() {
        cmd.env(k, v);
    }
    for k in SANDBOX_REMOVED {
        cmd.env_remove(k);
    }
}

pub fn isolate_pty(cmd: &mut portable_pty::CommandBuilder) {
    for (k, v) in sandbox_env() {
        cmd.env(k, v);
    }
    for k in SANDBOX_REMOVED {
        cmd.env_remove(k);
    }
}
