#![cfg(unix)]

//! Real-child lifecycle proofs for the agent-messaging MCP stdio server (pb-20261109-03).
//! Spawns the `patchbay --agent-messaging-mcp` binary and asserts start/exit evidence.

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}
const SIGTERM: i32 = 15;

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_patchbay"))
}

fn read_log(path: &PathBuf) -> String {
    fs::read_to_string(path).unwrap_or_default()
}

fn wait_for_log_contains(path: &PathBuf, needle: &str, timeout: Duration) -> String {
    let start = Instant::now();
    loop {
        let s = read_log(path);
        if s.contains(needle) {
            return s;
        }
        if start.elapsed() > timeout {
            return s;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[test]
fn real_child_sigterm_logs_signal_term_and_exits_by_signal() {
    let log = std::env::temp_dir().join(format!(
        "pb-mcp-sigterm-{}-{}.log",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis()
    ));
    let _ = fs::remove_file(&log);

    let mut child = Command::new(bin())
        .arg("--agent-messaging-mcp")
        .env("PATCHBAY_MCP_LOG_PATH", &log)
        .env("PATCHBAY_MCP_HOST_AGENT", "test-sigterm")
        .env("PATCHBAY_MESSAGE_SESSION", "sigterm-session")
        .env("PATCHBAY_MESSAGE_ADDR", "127.0.0.1:1")
        .env("PATCHBAY_MESSAGE_TOKEN", "dummy-token")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn patchbay --agent-messaging-mcp");
    // Keep stdin open so the child does not see EOF before we send the signal.
    let _child_stdin = child.stdin.take();

    // Wait for START so we know the child is running and has logged.
    let started = wait_for_log_contains(&log, "event=start", Duration::from_secs(10));
    assert!(started.contains("event=start"), "no start line: {started}");
    assert!(started.contains("session=sigterm-session"), "{started}");
    assert!(started.contains("host_agent=test-sigterm"), "{started}");

    // Allow tokio signal handlers to register (start is logged before run_stdio_async installs them).
    std::thread::sleep(Duration::from_millis(150));

    // Send SIGTERM.
    unsafe {
        let _ = kill(child.id() as i32, SIGTERM);
    }

    let status = child.wait().expect("wait");
    let content = read_log(&log);
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(
        status.signal(),
        Some(SIGTERM),
        "expected WIFSIGNALED/SIGTERM (re-raise semantics), got status={status:?} log={content:?} bin={:?}",
        bin()
    );

    // (a) signal_term line was written (write-then-re-raise, bounded).
    let content = wait_for_log_contains(&log, "cause=signal_term", Duration::from_secs(5));
    assert!(
        content.contains("event=exit") && content.contains("cause=signal_term"),
        "missing signal_term exit line: {content}"
    );
    assert!(!content.contains("dummy-token"), "token leaked: {content}");
    let _ = fs::remove_file(&log);
}

#[test]
fn real_child_logs_start_and_stdin_eof_exit() {
    // Blocker 3c: dropping the START log event call must fail this test.
    let log = std::env::temp_dir().join(format!(
        "pb-mcp-eof-{}-{}.log",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis()
    ));
    let _ = fs::remove_file(&log);

    let mut child = Command::new(bin())
        .arg("--agent-messaging-mcp")
        .env("PATCHBAY_MCP_LOG_PATH", &log)
        .env("PATCHBAY_MCP_HOST_AGENT", "test-eof")
        .env("PATCHBAY_MESSAGE_SESSION", "eof-session")
        .env("PATCHBAY_MESSAGE_ADDR", "127.0.0.1:1")
        .env("PATCHBAY_MESSAGE_TOKEN", "dummy-token")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn patchbay --agent-messaging-mcp");
    // Close stdin to trigger stdin_eof (drop write end).
    drop(child.stdin.take());

    let started = wait_for_log_contains(&log, "event=start", Duration::from_secs(10));
    assert!(started.contains("event=start"), "start line missing (dropping start log must fail): {started}");
    let status = child.wait().expect("wait");
    assert!(status.success(), "expected clean stdin_eof exit, got {status:?}");
    let content = wait_for_log_contains(&log, "cause=stdin_eof", Duration::from_secs(5));
    assert!(content.contains("event=exit") && content.contains("cause=stdin_eof"), "missing stdin_eof exit: {content}");
    assert!(!content.contains("dummy-token"), "token leaked: {content}");
    let _ = fs::remove_file(&log);
}
