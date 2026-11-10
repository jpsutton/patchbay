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

/// Manual live tests (`#[ignore]`: they talk to your real agents, logins and files) need the real
/// home, which the suite otherwise keeps out of reach. `PB_TEST_REAL_HOME=1 cargo test -- --ignored ...`
/// switches the isolation off: children inherit your environment, and `test_home()` is `$HOME`.
pub fn real_home_requested() -> bool {
    std::env::var_os("PB_TEST_REAL_HOME").is_some_and(|v| v == "1")
}

static SANDBOX: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

extern "C" fn remove_sandbox_at_exit() {
    if let Some(dir) = SANDBOX.get() {
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// One scratch HOME for every child process a test spawns (shells, agent CLIs, version probes).
/// Children write their own state under `$HOME` (`~/.config/opencode`, `~/.cursor`, shell history,
/// ...), which must never land in the developer's real home. Created once per test process and
/// removed again when the process exits (best effort). Always a temp dir, never the real home.
pub fn sandbox_home() -> &'static std::path::Path {
    SANDBOX.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("pb-test-home-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        // SAFETY: registers a plain function that only removes the directory created above.
        unsafe { libc::atexit(remove_sandbox_at_exit) };
        dir
    })
}

/// The home a test sees when nothing is pinned: the scratch home, or the real one on request.
pub fn test_home() -> PathBuf {
    if real_home_requested() {
        if let Some(h) = std::env::var_os("HOME") {
            return PathBuf::from(h);
        }
    }
    sandbox_home().to_path_buf()
}

/// Re-run one test of this binary in a child process with extra environment (e.g. a decoy that the
/// isolation must ignore) and require that it ran and passed.
pub fn run_this_test(name: &str, env: &[(&str, &std::path::Path)]) -> Result<(), String> {
    let mut cmd = std::process::Command::new(std::env::current_exe().unwrap());
    cmd.args(["--exact", name, "--test-threads=1"]);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    if out.status.success() && stdout.contains("1 passed") {
        Ok(())
    } else {
        Err(format!("{name} with {env:?}:\n{stdout}\n{}", String::from_utf8_lossy(&out.stderr)))
    }
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
    if real_home_requested() {
        return;
    }
    for (k, v) in sandbox_env() {
        cmd.env(k, v);
    }
    for k in SANDBOX_REMOVED {
        cmd.env_remove(k);
    }
}

pub fn isolate_tokio(cmd: &mut tokio::process::Command) {
    if real_home_requested() {
        return;
    }
    for (k, v) in sandbox_env() {
        cmd.env(k, v);
    }
    for k in SANDBOX_REMOVED {
        cmd.env_remove(k);
    }
}

pub fn isolate_pty(cmd: &mut portable_pty::CommandBuilder) {
    if real_home_requested() {
        return;
    }
    for (k, v) in sandbox_env() {
        cmd.env(k, v);
    }
    for k in SANDBOX_REMOVED {
        cmd.env_remove(k);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Child of `the_scratch_home_is_removed_when_the_process_exits`: only creates the scratch home.
    #[test]
    fn scratch_home_child() {
        assert!(sandbox_home().is_dir());
    }

    /// Nothing is left behind by a test process: the scratch home is gone once it has exited.
    #[test]
    fn the_scratch_home_is_removed_when_the_process_exits() {
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "test_support::tests::scratch_home_child"])
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let dir = std::env::temp_dir().join(format!("pb-test-home-{}", child.id()));
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success() && String::from_utf8_lossy(&out.stdout).contains("1 passed"));
        assert!(!dir.exists(), "{} was left behind", dir.display());
    }

    /// An exported HOME does not reach an unpinned test (child: see `claude_auth::tests`).
    #[test]
    fn an_exported_home_is_ignored() {
        let decoy = std::env::temp_dir().join(format!("pb-decoy-home-{}", crate::db::new_id()));
        std::fs::create_dir_all(&decoy).unwrap();
        let r = run_this_test("claude_auth::tests::unpinned_home_is_the_scratch_home", &[("HOME", &decoy)]);
        let _ = std::fs::remove_dir_all(&decoy);
        r.unwrap();
    }

    /// An exported XDG_CONFIG_HOME does not reach Cursor's login lookup.
    #[test]
    fn an_exported_xdg_config_home_is_ignored() {
        let decoy = std::env::temp_dir().join(format!("pb-decoy-xdg-{}", crate::db::new_id()));
        std::fs::create_dir_all(&decoy).unwrap();
        let r = run_this_test("code::cursor_plan::tests::login_paths_ignore_the_exported_xdg_config_home", &[("XDG_CONFIG_HOME", &decoy)]);
        let _ = std::fs::remove_dir_all(&decoy);
        r.unwrap();
    }

    /// The hatch works: with PB_TEST_REAL_HOME=1 a child sees the exported HOME instead of the scratch one.
    #[test]
    fn the_real_home_hatch_lets_children_inherit_home() {
        let decoy = std::env::temp_dir().join(format!("pb-decoy-hatch-{}", crate::db::new_id()));
        std::fs::create_dir_all(&decoy).unwrap();
        let r = run_this_test(
            "code::cursor_plan::tests::agent_status_inherits_home_under_the_hatch",
            &[("PB_TEST_REAL_HOME", std::path::Path::new("1")), ("HOME", &decoy)],
        );
        let _ = std::fs::remove_dir_all(&decoy);
        r.unwrap();
    }

    /// An exported GROK_HOME does not reach Grok's login lookup.
    #[test]
    fn an_exported_grok_home_is_ignored() {
        let decoy = std::env::temp_dir().join(format!("pb-decoy-grok-{}", crate::db::new_id()));
        std::fs::create_dir_all(&decoy).unwrap();
        let r = run_this_test("grok_auth::tests::grok_home_ignores_the_exported_variable", &[("GROK_HOME", &decoy)]);
        let _ = std::fs::remove_dir_all(&decoy);
        r.unwrap();
    }
}
