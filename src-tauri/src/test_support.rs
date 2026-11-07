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

/// Raise this process's open-file limit to what the system allows. A shell on macOS starts at
/// 256, too few for the mesh tests, which run several app instances with their runtimes,
/// databases and sockets in one process while cargo runs tests in parallel. Once per process.
pub fn raise_fd_limit() {
    #[cfg(unix)]
    {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            let mut lim = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
            // SAFETY: `lim` is a valid, writable rlimit for the duration of both calls.
            unsafe {
                if libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) != 0 {
                    return;
                }
                // Well above what the suite needs; macOS refuses more than OPEN_MAX (10240).
                let want = 8192.min(lim.rlim_max);
                if lim.rlim_cur < want {
                    lim.rlim_cur = want;
                    let _ = libc::setrlimit(libc::RLIMIT_NOFILE, &lim);
                }
            }
        });
    }
}
