#![cfg(unix)]

//! pb-20261107-34: MCP-child teardown fd return-to-baseline check (unix).
//!
//! **Scope (honest labeling):** proves the **MCP child teardown path only** — spawning N real
//! `patchbay --agent-messaging-mcp` stdio children, asserting N actually spawned (#356 trap:
//! N=0/short green cannot pass), observing host `/dev/fd` growth, then explicit teardown
//! (close parent stdin pipe handles, kill, wait, drop) and asserting host fd count returns
//! to baseline within bounded settle.
//!
//! **Not claimed:** app/session-lifecycle states (stopped, archived, process exited,
//! replaced agent, mid-turn) remain **unexercised** here — accepted and parked, not claimed.
//! Fix 2 (reaping) was closed as not-needed-until-demonstrated by advisor gate; os error 24
//! cause remains unidentified (recurrence runbook: capture lsof before restarting anything).

use std::fs;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_patchbay"))
}

fn count_fds() -> usize {
    // Host process fd count on unix (macOS/Linux): /dev/fd entries in this process.
    fs::read_dir("/dev/fd")
        .map(|d| d.filter_map(Result::ok).count())
        .unwrap_or_else(|e| panic!("failed to count /dev/fd: {e}"))
}

struct Spawned {
    id: usize,
    child: Child,
    log: PathBuf,
    stdin: Option<ChildStdin>,
}

fn spawn_mcp_child(id: usize) -> Spawned {
    let log = std::env::temp_dir().join(format!(
        "pb-fd-stress-{}-{}-{id}.log",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis()
    ));
    let _ = fs::remove_file(&log);
    let mut child = Command::new(bin())
        .arg("--agent-messaging-mcp")
        .env("PATCHBAY_MCP_LOG_PATH", &log)
        .env("PATCHBAY_MCP_HOST_AGENT", format!("fd-stress-{id}"))
        .env("PATCHBAY_MESSAGE_SESSION", format!("fd-stress-session-{id}"))
        .env("PATCHBAY_MESSAGE_ADDR", "127.0.0.1:1")
        .env("PATCHBAY_MESSAGE_TOKEN", "dummy-token")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap_or_else(|e| panic!("spawn patchbay --agent-messaging-mcp #{id}: {e}"));
    let stdin = child.stdin.take().expect("stdin piped");
    Spawned { id, child, log, stdin: Some(stdin) }
}

fn wait_started(s: &Spawned, timeout: Duration) -> String {
    let start = Instant::now();
    loop {
        let content = fs::read_to_string(&s.log).unwrap_or_default();
        if content.contains("event=start") {
            return content;
        }
        if start.elapsed() > timeout {
            panic!(
                "MCP child #{} (pid {:?}) did not log event=start within {:?}; log={:?}",
                s.id,
                s.child.id(),
                timeout,
                content
            );
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[test]
fn mcp_child_teardown_returns_fds_to_baseline() {
    const N: usize = 8;
    const START_TIMEOUT: Duration = Duration::from_secs(15);
    const EXIT_TIMEOUT: Duration = Duration::from_secs(10);
    const SETTLE: Duration = Duration::from_secs(5);
    const POLL: Duration = Duration::from_millis(100);
    // Bounded slack for transient fds (dir handles, /dev/fd enumeration) in the test process.
    const SLACK: usize = 4;

    let baseline = count_fds();
    eprintln!("pb-20261107-34 mcp_child_teardown: baseline_fds={baseline} N={N} bin={:?}", bin());

    // (1) Spawn N MCP child processes and assert N actually spawned (#356 trap).
    let mut spawned: Vec<Spawned> = Vec::with_capacity(N);
    for i in 0..N {
        spawned.push(spawn_mcp_child(i));
    }
    assert_eq!(
        spawned.len(),
        N,
        "spawned {} MCP children but expected N={N}; an N=0/short run passing green is the #356 trap",
        spawned.len()
    );
    for s in &spawned {
        let started = wait_started(s, START_TIMEOUT);
        assert!(started.contains("event=start"), "child #{} no start: {started}", s.id);
    }
    let pids: Vec<u32> = spawned.iter().map(|s| s.child.id()).collect();
    assert_eq!(pids.len(), N, "pid count != N");
    eprintln!("pb-20261107-34 mcp_child_teardown: spawned_and_started N={N} pids={pids:?}");

    let peak = count_fds();
    eprintln!("pb-20261107-34 mcp_child_teardown: peak_fds={peak} (delta +{})", peak.saturating_sub(baseline));
    // Must have grown: each child holds at least stdin pipe fds in the parent.
    assert!(
        peak >= baseline + N / 2,
        "fd count did not grow enough to evidence N={N} spawned MCP children (baseline={baseline}, peak={peak}); cannot trust this run"
    );

    // (2) MCP child teardown (explicit): close parent-held stdin, terminate+wait, drop.
    for s in &mut spawned {
        let _ = s.stdin.take(); // close parent end of stdin pipe
        let _ = s.child.kill();
    }
    let deadline = Instant::now() + EXIT_TIMEOUT;
    for s in &mut spawned {
        loop {
            if s.child.try_wait().expect("try_wait").is_some() {
                break;
            }
            if Instant::now() > deadline {
                panic!("MCP child #{} (pid {:?}) did not exit within {:?}", s.id, s.child.id(), EXIT_TIMEOUT);
            }
            std::thread::sleep(POLL);
        }
        let _ = fs::remove_file(&s.log);
    }
    drop(spawned);

    // (3) Assert host fd count returns to baseline within bounded settle time.
    let settle_deadline = Instant::now() + SETTLE;
    let mut last = count_fds();
    loop {
        if last <= baseline + SLACK {
            eprintln!("pb-20261107-34 mcp_child_teardown: fds_returned baseline={baseline} final={last} (slack={SLACK})");
            return;
        }
        if Instant::now() > settle_deadline {
            panic!(
                "pb-20261107-34 mcp_child_teardown: fd count did NOT return to baseline within {:?} (baseline={baseline}, peak={peak}, final={last}, N={N}). \
Evidence of MCP-child fd leak under the tested teardown path; report to desk. (This test covers MCP child teardown only, not app session-lifecycle states.)",
                SETTLE
            );
        }
        std::thread::sleep(POLL);
        last = count_fds();
    }
}
