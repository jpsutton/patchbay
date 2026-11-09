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

#[test]
fn real_child_fifo_log_path_exits_promptly_with_stdin_eof() {
    // Item 2: FIFO log path with no reader must not hang the child (open before runtime).
    // Child must exit promptly with stdin /dev/null (stdin_eof). Red-then-green: before fix,
    // open(O_APPEND) on FIFO blocks with no reader.
    let fifo = std::env::temp_dir().join(format!(
        "pb-mcp-fifo-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis()
    ));
    let _ = fs::remove_file(&fifo);
    let st = Command::new("mkfifo").arg(&fifo).status().expect("mkfifo");
    assert!(st.success(), "mkfifo failed: {st:?}");

    let child = Command::new(bin())
        .arg("--agent-messaging-mcp")
        .env("PATCHBAY_MCP_LOG_PATH", &fifo)
        .env("PATCHBAY_MCP_HOST_AGENT", "test-fifo")
        .env("PATCHBAY_MESSAGE_SESSION", "fifo-session")
        .env("PATCHBAY_MESSAGE_ADDR", "127.0.0.1:1")
        .env("PATCHBAY_MESSAGE_TOKEN", "dummy-token")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn patchbay --agent-messaging-mcp");

    // Must exit promptly (stdin is /dev/null => stdin_eof). If log open hangs, this times out.
    let start = Instant::now();
    let mut child = child;
    loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            assert!(status.success(), "expected prompt stdin_eof exit, got {status:?}");
            break;
        }
        if start.elapsed() > Duration::from_secs(6) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = fs::remove_file(&fifo);
            panic!("child hung on FIFO log path (did not exit within 6s)");
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let _ = fs::remove_file(&fifo);
}

#[test]
fn real_child_sigterm_during_inflight_tool_call_exits_by_signal() {
    // Item 1: SIGTERM must not be ignored while a tools/call is in flight (bridge never replies).
    // Dummy bridge accepts and never responds; child sends send_agent_message and blocks on read.
    // SIGTERM after call is in-flight must produce WIFSIGNALED within ~3s and signal_term log.
    use std::io::{BufRead, BufReader, Write as _};
    use std::net::TcpListener;

    let bridge = TcpListener::bind("127.0.0.1:0").expect("bind dummy bridge");
    let addr = bridge.local_addr().unwrap();
    let (req_tx, req_rx) = std::sync::mpsc::channel::<()>();
    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = bridge.accept() {
            // Read the request line (in-flight) then never reply.
            let mut buf = String::new();
            let _ = BufReader::new(&mut stream).read_line(&mut buf);
            let _ = req_tx.send(());
            std::thread::sleep(Duration::from_secs(30));
        }
    });

    let log = std::env::temp_dir().join(format!(
        "pb-mcp-inflight-{}-{}.log",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis()
    ));
    let _ = fs::remove_file(&log);

    let mut child = Command::new(bin())
        .arg("--agent-messaging-mcp")
        .env("PATCHBAY_MCP_LOG_PATH", &log)
        .env("PATCHBAY_MCP_HOST_AGENT", "test-inflight")
        .env("PATCHBAY_MESSAGE_SESSION", "inflight-session")
        .env("PATCHBAY_MESSAGE_ADDR", addr.to_string())
        .env("PATCHBAY_MESSAGE_TOKEN", "dummy-token")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn patchbay --agent-messaging-mcp");
    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    // Drain stdout so responses don't block.
    std::thread::spawn(move || {
        let mut r = BufReader::new(stdout);
        let mut line = String::new();
        while r.read_line(&mut line).unwrap_or(0) > 0 {
            line.clear();
        }
    });

    let started = wait_for_log_contains(&log, "event=start", Duration::from_secs(10));
    assert!(started.contains("event=start"), "no start: {started}");
    std::thread::sleep(Duration::from_millis(150));

    let init = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}"#;
    writeln!(stdin, "{init}").unwrap();
    stdin.flush().unwrap();
    std::thread::sleep(Duration::from_millis(100));

    let call = r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"send_agent_message","arguments":{"targetSessionId":"tgt","message":"hi"}}}"#;
    writeln!(stdin, "{call}").unwrap();
    stdin.flush().unwrap();

    // Wait until the dummy bridge has received the in-flight request.
    req_rx.recv_timeout(Duration::from_secs(5)).expect("bridge did not receive in-flight call");
    std::thread::sleep(Duration::from_millis(200));

    unsafe { let _ = kill(child.id() as i32, SIGTERM); }
    let deadline = Instant::now() + Duration::from_secs(3);
    let status = loop {
        if let Some(s) = child.try_wait().expect("try_wait") { break s; }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("SIGTERM ignored during in-flight tools/call (still alive after 3s)");
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(status.signal(), Some(SIGTERM), "expected WIFSIGNALED/SIGTERM, got {status:?}");
    let content = wait_for_log_contains(&log, "cause=signal_term", Duration::from_secs(5));
    assert!(content.contains("cause=signal_term"), "missing signal_term: {content}");
    assert!(!content.contains("dummy-token"), "token leaked: {content}");
    let _ = fs::remove_file(&log);
}
