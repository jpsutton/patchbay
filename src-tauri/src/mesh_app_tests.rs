//! Two app instances in one process, linked by a real mesh on localhost: moving a Code session,
//! checkpoints, taking over from an offline host, and remote terminals.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

use super::*;
use crate::mesh::keys::MemKeyStore;

const PASS: &str = "correct horse battery";

struct Node {
    state: AppState,
    dir: PathBuf,
    events: Arc<Mutex<Vec<(String, Value)>>>,
}

fn node(name: &str) -> Node {
    let dir = std::env::temp_dir().join(format!("pb-app-{name}-{}", db::new_id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = Arc::new(Db::open(&dir.join("db.sqlite")).unwrap());
    let mesh = Mesh::new(db.clone(), dir.clone(), Arc::new(MemKeyStore::default())).unwrap();
    mesh.set_config(Some(name), Some("127.0.0.1:0"), None).unwrap();
    let events = Arc::new(Mutex::new(Vec::new()));
    let ev = events.clone();
    let emit: mesh::node::UiSink = Arc::new(move |n: &str, p: Value| ev.lock().unwrap().push((n.to_owned(), p)));
    Node { state: AppState::new(db, dir.clone(), mesh, emit), dir, events }
}

async fn until(what: &str, secs: u64, mut f: impl FnMut() -> bool) {
    let t0 = std::time::Instant::now();
    while !f() {
        assert!(t0.elapsed() < Duration::from_secs(secs), "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn link(a: &Node, b: &Node) {
    a.state.mesh.create(PASS, "a").unwrap();
    until("listener", 5, || a.state.mesh.bound_port().is_some()).await;
    let url = format!("127.0.0.1:{}", a.state.mesh.bound_port().unwrap());
    b.state.mesh.join(&url, PASS, "b").unwrap();
    until("join request", 10, || !a.state.mesh.status()["pending"].as_array().unwrap().is_empty()).await;
    a.state.mesh.approve(b.state.mesh.node_id()).unwrap();
    until("membership", 10, || b.state.mesh.is_member()).await;
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn repo(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-q", "-b", "main"]);
    std::fs::write(dir.join("app.txt"), "v1\n").unwrap();
    git(dir, &["add", "."]);
    git(dir, &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", "first"]);
}

fn host_of(n: &Node, id: &str) -> Option<String> {
    n.state.db.get_code_session(id).unwrap().and_then(|s| s.host_node)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn move_checkpoint_take_over_and_orphaned_work() {
    let (a, b) = (node("alpha"), node("bravo"));
    link(&a, &b).await;
    let (an, bn) = (a.state.mesh.node_id().to_owned(), b.state.mesh.node_id().to_owned());

    // A session on A with uncommitted and untracked work.
    let proj_a = a.dir.join("proj");
    repo(&proj_a);
    std::fs::write(proj_a.join("app.txt"), "v2, not committed\n").unwrap();
    std::fs::write(proj_a.join("notes.txt"), "untracked\n").unwrap();
    let s = a.state.db.create_code_session(&proj_a.to_string_lossy(), None, "grok").unwrap();
    until("session reaches B", 10, || b.state.db.get_code_session(&s.id).unwrap().is_some()).await;

    // Move it to B.
    let proj_b = b.dir.join("proj");
    let note = import_local(&b.state, &s.id, &proj_b.to_string_lossy()).await.unwrap();
    assert!(note.is_none());
    assert_eq!(std::fs::read_to_string(proj_b.join("app.txt")).unwrap(), "v2, not committed\n");
    assert_eq!(std::fs::read_to_string(proj_b.join("notes.txt")).unwrap(), "untracked\n");
    assert_eq!(git(&proj_b, &["symbolic-ref", "--short", "HEAD"]), "main");
    assert_eq!(host_of(&b, &s.id).as_deref(), Some(bn.as_str()));
    until("A sees the new host", 10, || host_of(&a, &s.id).as_deref() == Some(bn.as_str())).await;
    until("A's move settles", 10, || !a.state.code.is_moving(&s.id)).await;
    // No transcript and no agent state came along, so the first prompt gets a hand-off note.
    assert!(b.state.db.get_setting(&format!("mesh.handoff.{}", s.id)).unwrap().is_some());
    // A's checkout is untouched.
    assert_eq!(std::fs::read_to_string(proj_a.join("app.txt")).unwrap(), "v2, not committed\n");

    // A turn on B ends: A fetches a checkpoint.
    std::fs::write(proj_b.join("app.txt"), "v3 from bravo\n").unwrap();
    code_sink(&b.state)(code::acp::Envelope {
        code_session_id: s.id.clone(),
        event: code::acp::CodeEvent::PromptDone { stop_reason: json!("end_turn") },
    });
    until("checkpoint on A", 20, || handoff::checkpoint(&a.dir, &s.id).is_some()).await;

    // B vanishes; A takes over from the checkpoint into a fresh folder.
    b.state.mesh.stop();
    until("A notices B is gone", 40, || !a.state.mesh.connected(&bn)).await;
    let proj_a2 = a.dir.join("proj2");
    take_over_local(&a.state, &s.id, &proj_a2.to_string_lossy()).await.unwrap();
    assert_eq!(std::fs::read_to_string(proj_a2.join("app.txt")).unwrap(), "v3 from bravo\n");
    assert_eq!(host_of(&a, &s.id).as_deref(), Some(an.as_str()));
    let epoch = a.state.db.get_code_session(&s.id).unwrap().unwrap().epoch;
    assert_eq!(epoch, 2);

    // B did more work while cut off, then comes back: it keeps that work on an orphaned ref.
    std::fs::write(proj_b.join("app.txt"), "v4, only on bravo\n").unwrap();
    b.state.mesh.start();
    until("B learns it lost the session", 30, || host_of(&b, &s.id).as_deref() == Some(an.as_str())).await;
    until("orphaned ref on B", 10, || !git(&proj_b, &["for-each-ref", "refs/patchbay/orphaned"]).is_empty()).await;
    let r = git(&proj_b, &["for-each-ref", "--format=%(refname)", "refs/patchbay/orphaned"]);
    assert_eq!(git(&proj_b, &["show", &format!("{r}:app.txt")]), "v4, only on bravo");
    assert!(b.events.lock().unwrap().iter().any(|(n, p)| n == code::acp::EVENT && p["event"]["type"] == "notice"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remote_session_commands_and_terminal() {
    let (a, b) = (node("alpha2"), node("bravo2"));
    link(&a, &b).await;
    let an = a.state.mesh.node_id().to_owned();
    let proj = a.dir.join("proj");
    repo(&proj);
    let s = a.state.db.create_code_session(&proj.to_string_lossy(), None, "grok").unwrap();
    until("session reaches B", 10, || b.state.db.get_code_session(&s.id).unwrap().is_some()).await;

    // B sees the session as remote and asks A about it.
    assert_eq!(remote_host(&b.state, &s.id).unwrap().as_deref(), Some(an.as_str()));
    let files: Vec<String> = serde_json::from_value(b.state.mesh.rpc(&an, "code.files", json!({ "id": s.id })).await.unwrap()).unwrap();
    assert!(files.iter().any(|f| f == "app.txt"), "{files:?}");
    let attach = b.state.mesh.rpc(&an, "code.attach", json!({ "id": s.id })).await.unwrap();
    assert_eq!(attach["running"], false);
    // A refuses commands for sessions it doesn't host.
    let err = a.state.mesh.rpc(b.state.mesh.node_id(), "code.start", json!({ "id": s.id })).await.unwrap_err();
    assert!(err.to_string().contains("runs on"), "{err}");

    // A shell on A, driven from B.
    let p = json!({ "termId": "t1", "sessionId": s.id, "cols": 80, "rows": 24 });
    b.state.mesh.rpc(&an, "term.open", p).await.unwrap();
    b.state.mesh.rpc(&an, "term.write", json!({ "termId": "t1", "data": "echo mesh-$((40+2))\n" })).await.unwrap();
    let output = || {
        use base64::Engine;
        let ev = b.events.lock().unwrap();
        let bytes: Vec<u8> = ev
            .iter()
            .filter(|(n, p)| n == "mesh://term" && p["termId"] == "t1")
            .filter_map(|(_, p)| p["event"]["data"].as_str())
            .flat_map(|d| base64::engine::general_purpose::STANDARD.decode(d).unwrap_or_default())
            .collect();
        String::from_utf8_lossy(&bytes).into_owned()
    };
    until("terminal output on B", 15, || output().contains("mesh-42")).await;
    b.state.mesh.rpc(&an, "term.close", json!({ "termId": "t1" })).await.unwrap();
}
