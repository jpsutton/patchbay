//! What the tests need from the machine they run on.

use std::path::PathBuf;

/// A fresh folder under the temp dir whose path leaves room for a Unix socket inside it: macOS
/// puts `temp_dir()` under `/var/folders/…` (about 50 characters) and caps a socket path at
/// 104, which a full uuid in the name overruns.
pub fn short_dir(tag: &str) -> PathBuf {
    let id = crate::db::new_id();
    std::env::temp_dir().join(format!("pb-{tag}-{}", &id[..8]))
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
