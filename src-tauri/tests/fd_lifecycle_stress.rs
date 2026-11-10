#![cfg(unix)]

//! pb-20261107-34/pb-20261109-08: MCP-child teardown fd return-to-baseline check (unix).
//!
//! **Scope (honest labeling):** proves the **MCP child teardown path only** — spawning N real
//! `patchbay --agent-messaging-mcp` stdio children, asserting N actually spawned with
//! **run-measuring guards** (not tautologies: `peak-baseline >= N`, `N>0`, and OS liveness via
//! `kill(pid,0)` for each recorded pid before teardown), observing host `/dev/fd` growth, then
//! explicit teardown (close parent stdin pipe handles, kill, wait, drop) and asserting host fd
//! count returns to baseline within bounded settle.
//!
//! **Not claimed:** app/session-lifecycle states (stopped, archived, process exited,
//! replaced agent, mid-turn) remain **unexercised** here — accepted and parked, not claimed.
//! Fix 2 (reaping) was closed as not-needed-until-demonstrated by advisor gate; os error 24
//! cause remains unidentified (recurrence runbook: capture lsof before restarting anything).

use std::fs;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_patchbay"))
}

fn count_fds() -> usize {
    // Host process fd count on unix (macOS/Linux): /dev/fd entries in this process.
    fs::read_dir("/dev/fd")
        .map(|d| d.filter_map(Result::ok).count())
        .unwrap_or_else(|e| panic!("failed to count /dev/fd: {e}"))
}

fn os_pid_alive(pid: u32) -> bool {
    // kill(pid,0): probes liveness from the OS (not from a vec the test filled).
    unsafe { kill(pid as i32, 0) == 0 }
}

/// pb-20261109-08 #356 guard: measures the run, not the code.
/// Named assertions so plant proofs show which guard fired.
fn assert_n_spawned_measured(n: usize, baseline_fds: usize, peak_fds: usize, pids: &[u32]) {
    assert!(
        n > 0,
        "pb-20261109-08 guard(n-nonzero): N must be a nonzero const; got N={n} (N=0 would make fd/pid checks vacuous)"
    );
    let delta = peak_fds.saturating_sub(baseline_fds);
    assert!(
        delta >= n,
        "pb-20261109-08 guard(fd-delta): peak-baseline fd delta must evidence N spawned children (>=1 fd each); N={n} baseline={baseline_fds} peak={peak_fds} delta={delta}"
    );
    assert_eq!(
        pids.len(),
        n,
        "pb-20261109-08 guard(pid-count): recorded pid count {} != N={n}",
        pids.len()
    );
    for &pid in pids {
        assert!(
            os_pid_alive(pid),
            "pb-20261109-08 guard(os-liveness): recorded pid {pid} is not live per kill(pid,0) before teardown (pid never kept/already dead; cannot trust run)"
        );
    }
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

    // (1) Spawn N MCP child processes and wait for start; run-measuring guard follows (#356/pb-20261109-08).
    let mut spawned: Vec<Spawned> = Vec::with_capacity(N);
    for i in 0..N {
        spawned.push(spawn_mcp_child(i));
    }
    for s in &spawned {
        let started = wait_started(s, START_TIMEOUT);
        assert!(started.contains("event=start"), "child #{} no start: {started}", s.id);
    }
    let pids: Vec<u32> = spawned.iter().map(|s| s.child.id()).collect();
    eprintln!("pb-20261107-34 mcp_child_teardown: spawned_and_started N={N} pids={pids:?}");

    let peak = count_fds();
    eprintln!("pb-20261107-34 mcp_child_teardown: peak_fds={peak} (delta +{})", peak.saturating_sub(baseline));
    // pb-20261109-08: measure the run (not tautologies). N>0, peak-baseline>=N, OS liveness
    // of recorded pids (kill(pid,0)) before teardown.
    assert_n_spawned_measured(N, baseline, peak, &pids);

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

#[test]
fn pb_20261109_08_plant_n0_guard_is_red() {
    // #356 trap plant: N=0. A vacuous guard would pass; our guard must fail with a named assertion.
    let r = std::panic::catch_unwind(|| assert_n_spawned_measured(0, 4, 4, &[]));
    let msg = match r {
        Err(p) => {
            if let Some(s) = p.downcast_ref::<String>() {
                s.clone()
            } else if let Some(s) = p.downcast_ref::<&str>() {
                (*s).to_string()
            } else {
                "<non-string panic>".to_string()
            }
        }
        Ok(()) => panic!("pb-20261109-08 plant N=0: guard PASSED (green) but must be RED"),
    };
    eprintln!("pb-20261109-08 plant N=0: caught RED panic: {msg}");
    assert!(
        msg.contains("pb-20261109-08 guard(n-nonzero)") || msg.contains("N must be a nonzero"),
        "named assertion missing from panic: {msg}"
    );
}

#[test]
fn pb_20261109_08_plant_dead_recorded_pid_guard_is_red() {
    // Plant: record pids of children that were spawned but are no longer live (not kept/dead).
    // OS-liveness (kill(pid,0)) must fail with a named assertion; vec contents alone are insufficient.
    let mut c = spawn_mcp_child(0);
    let _started = wait_started(&c, Duration::from_secs(15));
    let pid = c.child.id();
    let _ = c.stdin.take();
    let _ = c.child.kill();
    let _ = c.child.wait();
    let _ = fs::remove_file(&c.log);
    // pid is now dead; give the OS a moment to reflect exit.
    std::thread::sleep(Duration::from_millis(50));
    assert!(!os_pid_alive(pid), "plant setup: pid {pid} should be dead after kill/wait");

    let r = std::panic::catch_unwind(|| {
        // Satisfy N>0 and fd-delta guards to isolate OS-liveness; pids contains the dead pid.
        assert_n_spawned_measured(1, 0, 10, &[pid])
    });
    let msg = match r {
        Err(p) => {
            if let Some(s) = p.downcast_ref::<String>() {
                s.clone()
            } else if let Some(s) = p.downcast_ref::<&str>() {
                (*s).to_string()
            } else {
                "<non-string panic>".to_string()
            }
        }
        Ok(()) => panic!("pb-20261109-08 plant dead-pid: guard PASSED (green) but must be RED"),
    };
    eprintln!("pb-20261109-08 plant dead-pid: caught RED panic (pid={pid}): {msg}");
    assert!(
        msg.contains("pb-20261109-08 guard(os-liveness)") && msg.contains(&pid.to_string()),
        "named OS-liveness assertion/pid missing from panic: {msg}"
    );
}
