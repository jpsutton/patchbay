//! Two app instances in one process, linked by a real mesh on localhost: moving a Code session,
//! taking over from an offline host, and remote terminals.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

use super::*;
use crate::mesh::keys::MemKeyStore;
use crate::mesh::members;

const PASS: &str = "correct horse battery";

struct Node {
    state: AppState,
    dir: PathBuf,
    events: Arc<Mutex<Vec<(String, Value)>>>,
}

fn node(name: &str) -> Node {
    crate::test_support::raise_fd_limit();
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
    a.state.mesh.approve(b.state.mesh.node_id()).await.unwrap();
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
async fn move_take_over_and_orphaned_work() {
    let (a, b) = (node("alpha"), node("bravo"));
    link(&a, &b).await;
    let (an, bn) = (a.state.mesh.node_id().to_owned(), b.state.mesh.node_id().to_owned());

    // A session on A with uncommitted and untracked work.
    let proj_a = a.dir.join("proj");
    repo(&proj_a);
    std::fs::write(proj_a.join("app.txt"), "v2, not committed\n").unwrap();
    std::fs::write(proj_a.join("notes.txt"), "untracked\n").unwrap();
    let s = a.state.db.create_code_session(&proj_a.to_string_lossy(), None, "grok", "default").unwrap();
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

    // A turn on B ends. Nothing is copied to A: the working folder only moves when asked.
    std::fs::write(proj_b.join("app.txt"), "v3 from bravo\n").unwrap();
    code_sink(&b.state)(code::acp::Envelope {
        code_session_id: s.id.clone(),
        event: code::acp::CodeEvent::PromptDone { stop_reason: json!("end_turn") },
    });
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(!a.dir.join("mesh").join("mirrors").exists());
    assert!(!a.dir.join("mesh").join("checkpoints").exists());

    // B vanishes; A takes over with the conversation only, in a folder it already has.
    b.state.mesh.stop();
    until("A notices B is gone", 40, || !a.state.mesh.connected(&bn)).await;
    let proj_a2 = a.dir.join("proj2");
    assert!(take_over_local(&a.state, &s.id, &proj_a2.to_string_lossy()).await.is_err(), "the folder must exist");
    std::fs::create_dir_all(&proj_a2).unwrap();
    let note = take_over_local(&a.state, &s.id, &proj_a2.to_string_lossy()).await.unwrap();
    assert!(note.is_some());
    assert!(!proj_a2.join("app.txt").exists());
    assert!(a.state.db.get_setting(&format!("mesh.handoff.{}", s.id)).unwrap().is_some());
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
async fn desktop_starts_a_session_in_a_remote_folder() {
    let (a, b) = (node("alpha7"), node("bravo7"));
    link(&a, &b).await;
    let an = a.state.mesh.node_id().to_owned();
    let proj = a.dir.join("proj");
    repo(&proj);
    let cwd = proj.to_string_lossy().into_owned();

    // Started from B with A as host, it runs on A, and B has its row when the call returns.
    let s = create_code_session(&b.state, cwd.clone(), None, Some("grok".into()), Some(an.clone())).await.unwrap();
    assert_eq!(s.host_node.as_deref(), Some(an.as_str()));
    assert_eq!(s.cwd, cwd);
    assert_eq!(host_of(&a, &s.id).as_deref(), Some(an.as_str()));
    assert_eq!(host_of(&b, &s.id).as_deref(), Some(an.as_str()));

    // A folder picker's trailing slash is not part of the folder.
    let slashed = create_code_session(&b.state, format!("{cwd}/"), None, Some("grok".into()), Some(an.clone())).await.unwrap();
    assert_eq!(slashed.cwd, cwd);

    // Naming this computer as host is the same as naming none.
    let bn = b.state.mesh.node_id().to_owned();
    let bproj = b.dir.join("proj");
    repo(&bproj);
    let s = create_code_session(&b.state, bproj.to_string_lossy().into_owned(), None, None, Some(bn.clone())).await.unwrap();
    assert_eq!(s.host_node.as_deref(), Some(bn.as_str()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remote_session_commands_and_terminal() {
    let (a, b) = (node("alpha2"), node("bravo2"));
    link(&a, &b).await;
    let an = a.state.mesh.node_id().to_owned();
    let proj = a.dir.join("proj");
    repo(&proj);
    let s = a.state.db.create_code_session(&proj.to_string_lossy(), None, "grok", "default").unwrap();
    until("session reaches B", 10, || b.state.db.get_code_session(&s.id).unwrap().is_some()).await;

    // B sees the session as remote and asks A about it.
    assert_eq!(remote_host(&b.state, &s.id).unwrap().as_deref(), Some(an.as_str()));
    let files: Vec<String> = serde_json::from_value(b.state.mesh.rpc(&an, "code.files", json!({ "id": s.id })).await.unwrap()).unwrap();
    assert!(files.iter().any(|f| f == "app.txt"), "{files:?}");
    let attach = b.state.mesh.rpc(&an, "code.attach", json!({ "id": s.id })).await.unwrap();
    assert_eq!(attach["running"], false);
    // A forced send from B checks first that A has a running agent that can take one.
    let err = code_prompt(&b.state, s.id.clone(), "now".into(), vec![], None, None, Some(true)).await.unwrap_err();
    assert!(err.0.contains("isn't running"), "{}", err.0);
    let err = code_send_now(&b.state, s.id.clone(), "queued".into()).await.unwrap_err();
    assert!(err.0.contains("isn't running"), "{}", err.0);
    // A sign-in from B goes to A, which has no agent running for the session yet.
    let err = code_authenticate(&b.state, s.id.clone(), "oauth".into(), Some("k".into())).await.unwrap_err();
    assert!(err.0.contains("agent is not running"), "{}", err.0);
    // A refuses commands for sessions it doesn't host.
    let err = a.state.mesh.rpc(b.state.mesh.node_id(), "code.start", json!({ "id": s.id })).await.unwrap_err();
    assert!(err.to_string().contains("runs on"), "{err}");

    // Before its first message, B can give the session another agent; the old agent's ids go.
    a.state.db.set_code_session_agent_model(&s.id, "old-model").unwrap();
    let p = json!({ "id": s.id, "agent": "claude-code", "modelRef": "m-x" });
    b.state.mesh.rpc(&an, "code.set_agent", p).await.unwrap();
    let got = a.state.db.get_code_session(&s.id).unwrap().unwrap();
    assert_eq!((got.agent.as_str(), got.agent_model, got.acp_session_id), ("claude-code", None, None));
    assert_eq!(got.model_ref.as_deref(), Some("m-x"));
    let err = b.state.mesh.rpc(&an, "code.set_agent", json!({ "id": s.id, "agent": "nope" })).await.unwrap_err();
    assert!(err.to_string().contains("unknown coding agent"), "{err}");
    // After one, the old turns become earlier history and the next prompt carries them.
    a.state.db.set_code_transcript(&s.id, r#"{"v":1,"entries":[{"kind":"user","text":"hi"}],"toolCalls":{}}"#).unwrap();
    b.state.mesh.rpc(&an, "code.set_agent", json!({ "id": s.id, "agent": "grok" })).await.unwrap();
    assert_eq!(a.state.db.get_code_session(&s.id).unwrap().unwrap().agent, "grok");
    let t: serde_json::Value = serde_json::from_str(&a.state.db.code_transcript(&s.id).unwrap().unwrap()).unwrap();
    assert_eq!(t["entries"], json!([]));
    assert_eq!(t["earlier"]["entries"][0]["text"], "hi");
    let note = a.state.db.get_setting(&format!("mesh.handoff.{}", s.id)).unwrap().unwrap();
    assert!(note.contains("taking over") && note.contains("User: hi"), "{note}");

    // B only views the session: its partial transcript must not replace the host's cache.
    save_code_transcript(&b.state, s.id.clone(), r#"{"v":1,"entries":[],"toolCalls":{}}"#.into()).await.unwrap();
    assert_eq!(b.state.db.code_transcript(&s.id).unwrap(), None);
    save_code_transcript(&a.state, s.id.clone(), r#"{"v":1,"entries":[{"kind":"user","text":"host"}],"toolCalls":{}}"#.into())
        .await
        .unwrap();
    assert!(a.state.db.code_transcript(&s.id).unwrap().unwrap().contains("host"));

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

/// A phone's view of the mesh: what reached its app.
struct Phone {
    client: patchbay_mesh::client::RemoteClient,
    events: Arc<Mutex<Vec<(String, Value)>>>,
}

impl Phone {
    fn count(&self, f: impl Fn(&str, &Value) -> bool) -> usize {
        self.events.lock().unwrap().iter().filter(|(n, p)| f(n, p)).count()
    }

    fn texts(&self, session: &str) -> Vec<String> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter(|(n, p)| n == code::acp::EVENT && p["codeSessionId"] == session)
            .filter_map(|(_, p)| p["event"]["update"]["content"]["text"].as_str().map(str::to_owned))
            .collect()
    }
}

fn phone() -> Phone {
    crate::test_support::raise_fd_limit();
    let dir = std::env::temp_dir().join(format!("pb-phone-{}", db::new_id()));
    let events = Arc::new(Mutex::new(Vec::new()));
    let ev = events.clone();
    let sink: patchbay_mesh::client::Sink = Arc::new(move |n: &str, p: Value| ev.lock().unwrap().push((n.to_owned(), p)));
    let client = patchbay_mesh::client::RemoteClient::new(dir, Arc::new(MemKeyStore::default()), sink).unwrap();
    client.start();
    Phone { client, events }
}

/// A phone that leaves tells its gateway, so the computers drop it from their lists. It keeps
/// its keys: joining again must raise a request to approve, not resume its old session.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn phone_rejoins_after_leaving() {
    let a = node("alpha6");
    a.state.mesh.create(PASS, "a").unwrap();
    until("listener", 5, || a.state.mesh.bound_port().is_some()).await;
    let url = format!("127.0.0.1:{}", a.state.mesh.bound_port().unwrap());
    let p = phone();
    let pid = p.client.node_id();
    let requested = || a.state.mesh.status()["pending"].as_array().unwrap().iter().any(|r| r["nodeId"] == pid);
    p.client.join(&url, PASS, "pocket").unwrap();
    until("phone's join request", 10, requested).await;
    a.state.mesh.approve(&pid).await.unwrap();
    until("phone online", 10, || p.client.status()["phase"] == "online").await;

    p.client.leave();
    until("A learns the phone left", 10, || members::get(&a.state.db, &pid).unwrap().unwrap().has_left()).await;
    assert!(!a.state.mesh.status()["members"].as_array().unwrap().iter().any(|m| m["nodeId"] == pid));
    p.client.join(&url, PASS, "pocket").unwrap();
    until("phone's second join request", 10, requested).await;
    a.state.mesh.approve(&pid).await.unwrap();
    until("phone admitted again", 10, || p.client.is_member()).await;
    until("phone online again", 10, || p.client.status()["phase"] == "online").await;
}

fn emit(n: &Node, id: &str, event: code::acp::CodeEvent) {
    code_sink(&n.state)(code::acp::Envelope { code_session_id: id.to_owned(), event });
}

fn chunk(text: &str) -> code::acp::CodeEvent {
    code::acp::CodeEvent::Update {
        update: json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": text } }),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn phone_joins_follows_resumes_and_switches_gateway() {
    let (a, b) = (node("alpha3"), node("bravo3"));
    link(&a, &b).await;
    let proj = a.dir.join("proj");
    repo(&proj);
    let s = a.state.db.create_code_session(&proj.to_string_lossy(), None, "grok", "default").unwrap();
    until("session reaches B", 10, || b.state.db.get_code_session(&s.id).unwrap().is_some()).await;

    // The phone asks A to join; a person approves it there.
    let p = phone();
    let url = format!("127.0.0.1:{}", a.state.mesh.bound_port().unwrap());
    p.client.join(&url, PASS, "pocket").unwrap();
    let pid = p.client.node_id();
    until("phone's join request", 10, || {
        a.state.mesh.status()["pending"].as_array().unwrap().iter().any(|r| r["nodeId"] == pid && r["role"] == "remote")
    })
    .await;
    a.state.mesh.approve(&pid).await.unwrap();
    until("phone admitted", 10, || p.client.is_member()).await;
    until("phone online", 10, || p.client.status()["phase"] == "online").await;
    let rec = members::get(&a.state.db, &pid).unwrap().unwrap();
    assert!(rec.is_remote());
    // The record syncs to B like any member's; the phone never got the data key.
    until("B knows the phone", 10, || members::get(&b.state.db, &pid).unwrap().is_some()).await;
    // A is the gateway; B still shows the phone as relayed through A, not offline.
    let an = a.state.mesh.node_id().to_owned();
    until("phone direct on A", 10, || {
        a.state.mesh.status()["members"].as_array().unwrap().iter().any(|m| m["nodeId"] == pid && m["state"] == "direct")
    })
    .await;
    until("phone relayed on B", 10, || {
        b.state.mesh.status()["members"].as_array().unwrap().iter().any(|m| {
            m["nodeId"] == pid && m["state"] == "relay" && m["via"] == an.as_str()
        })
    })
    .await;

    // Lists come from the gateway; the phone learns B's address for later.
    let idx = p.client.rpc("remote.index", Value::Null).await.unwrap();
    assert!(idx["codeSessions"].as_array().unwrap().iter().any(|c| c["id"] == s.id));
    // The sidebar layout comes with the lists, for the phone's groups.
    assert!(idx["sidebarLayout"].is_array());
    until("phone knows two computers", 10, || {
        p.client.status()["members"].as_array().unwrap().iter().filter(|m| m["role"].is_null()).count() == 2
    })
    .await;

    // Follow the session: a snapshot first, then live events with points.
    let topic = format!("session:{}", s.id);
    let snap = p.client.subscribe(&topic, true).await.unwrap();
    assert_eq!(snap["mode"], "snapshot");
    emit(&a, &s.id, code::acp::CodeEvent::Status { state: "starting".into(), message: None });
    emit(&a, &s.id, chunk("Hel"));
    emit(&a, &s.id, chunk("lo"));
    until("live events on the phone", 10, || p.texts(&s.id).len() == 2).await;

    // The network drops; the agent keeps going meanwhile.
    a.state.mesh.drop_remote(&pid);
    emit(&a, &s.id, chunk(" wor"));
    emit(&a, &s.id, chunk("ld"));
    emit(&a, &s.id, code::acp::CodeEvent::PromptDone { stop_reason: json!("end_turn") });
    until("phone back online", 15, || p.client.status()["phase"] == "online" && p.client.status()["gateway"].is_string()).await;
    until("missed events caught up", 10, || p.count(|n, e| n == code::acp::EVENT && e["event"]["type"] == "promptDone") == 1).await;
    // Resumed with its ticket, and the missed chunks came merged, with no snapshot needed.
    assert_eq!(p.client.status()["resumed"], true);
    assert_eq!(p.texts(&s.id).concat(), "Hello world");
    assert_eq!(p.count(|n, e| n == patchbay_mesh::client::EV_RESYNC && e["topic"] == topic), 0);

    // A goes away: the phone finds B, and its point still resumes there.
    until("B saw the turn end", 10, || {
        b.events.lock().unwrap().iter().any(|(n, e)| n == code::acp::EVENT && e["event"]["type"] == "promptDone")
    })
    .await;
    a.state.mesh.stop();
    let bn = b.state.mesh.node_id().to_owned();
    until("phone on B", 30, || p.client.status()["gateway"] == bn.as_str()).await;
    let idx = p.client.rpc("remote.index", Value::Null).await.unwrap();
    assert!(idx["codeSessions"].as_array().unwrap().iter().any(|c| c["id"] == s.id));
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(p.count(|n, e| n == patchbay_mesh::client::EV_RESYNC && e["topic"] == topic), 0);
    assert_eq!(p.texts(&s.id).concat(), "Hello world");

    // Archived from the phone through B while A, its host, is gone: nothing reachable to stop, and
    // the archive still holds. Restoring brings it back.
    let p_arch = json!({ "id": s.id, "archived": true, "pinned": true });
    p.client.rpc("remote.update_code_session", p_arch).await.unwrap();
    until("archived on B", 10, || {
        b.state.db.get_code_session(&s.id).unwrap().unwrap().archived_at.is_some()
    })
    .await;
    assert!(b.state.db.get_code_session(&s.id).unwrap().unwrap().pinned);
    p.client.rpc("remote.update_code_session", json!({ "id": s.id, "archived": false })).await.unwrap();
    assert!(b.state.db.get_code_session(&s.id).unwrap().unwrap().archived_at.is_none());

    // Removed on B: the phone is told and forgets the mesh.
    b.state.mesh.remove(&pid).unwrap();
    until("phone forgets the mesh", 10, || !p.client.is_member()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn snapshot_leaves_out_a_turn_cut_off_from_its_host() {
    let (a, b) = (node("alpha8"), node("bravo8"));
    link(&a, &b).await;
    let an = a.state.mesh.node_id().to_owned();
    let proj = a.dir.join("proj");
    repo(&proj);
    let s = a.state.db.create_code_session(&proj.to_string_lossy(), None, "grok", "default").unwrap();
    until("session reaches B", 10, || b.state.db.get_code_session(&s.id).unwrap().is_some()).await;

    // The phone uses B as its gateway.
    let p = phone();
    let url = format!("127.0.0.1:{}", b.state.mesh.bound_port().unwrap());
    p.client.join(&url, PASS, "pocket").unwrap();
    let pid = p.client.node_id();
    until("phone's join request", 10, || {
        b.state.mesh.status()["pending"].as_array().unwrap().iter().any(|r| r["nodeId"] == pid)
    })
    .await;
    b.state.mesh.approve(&pid).await.unwrap();
    until("phone online", 10, || p.client.status()["phase"] == "online").await;

    // A turn on A, half done. While A is there, a snapshot through B carries it.
    emit(&a, &s.id, code::acp::CodeEvent::Status { state: "starting".into(), message: None });
    emit(&a, &s.id, chunk("half a turn"));
    let half = |snap: &Value| {
        snap["tail"].as_array().unwrap().iter().any(|e| e[1]["event"]["update"]["content"]["text"] == "half a turn")
    };
    until("B has the turn so far", 10, || {
        b.events.lock().unwrap().iter().any(|(_, e)| e["event"]["update"]["content"]["text"] == "half a turn")
    })
    .await;
    let topic = format!("session:{}", s.id);
    let snap = p.client.subscribe(&topic, true).await.unwrap();
    assert!(half(&snap), "{snap}");

    // B loses A. The turn may finish and reach the saved transcript meanwhile, so B's copy of
    // its first half no longer goes into a snapshot.
    a.state.mesh.stop();
    until("B notices A is gone", 40, || !b.state.mesh.connected(&an)).await;
    let snap = p.client.subscribe(&topic, true).await.unwrap();
    assert_eq!(snap["mode"], "snapshot");
    assert!(!half(&snap), "{snap}");
    assert_eq!(snap["covered"], false);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn phone_starts_a_session_on_another_computer() {
    let (a, b) = (node("alpha4"), node("bravo4"));
    link(&a, &b).await;
    let p = phone();
    let url = format!("127.0.0.1:{}", a.state.mesh.bound_port().unwrap());
    p.client.join(&url, PASS, "pocket").unwrap();
    let pid = p.client.node_id();
    until("phone's join request", 10, || {
        a.state.mesh.status()["pending"].as_array().unwrap().iter().any(|r| r["nodeId"] == pid)
    })
    .await;
    a.state.mesh.approve(&pid).await.unwrap();
    until("phone online", 10, || p.client.status()["phase"] == "online").await;
    let bn = b.state.mesh.node_id().to_owned();

    // What B offers, asked through A.
    let opts = p.client.rpc("remote.new_session_options", json!({ "host": bn })).await.unwrap();
    assert!(opts["agents"].as_array().unwrap().iter().any(|x| x["id"] == "patchbay"));
    assert_eq!(opts["defaultAgent"], "patchbay");

    // Which build B runs, for the session info dialog.
    let info = p.client.rpc("remote.host_info", json!({ "host": bn })).await.unwrap();
    assert_eq!(info["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(info["os"], std::env::consts::OS);
    assert_eq!(info["arch"], std::env::consts::ARCH);

    // Browsing stays inside the home folder, and lists only visible folders.
    let home = p.client.rpc("remote.list_dirs", json!({ "host": bn })).await.unwrap();
    assert_eq!(home["path"], home["home"]);
    assert!(home["parent"].is_null());
    assert!(home["dirs"].as_array().unwrap().iter().all(|d| !d.as_str().unwrap().starts_with('.')));
    let err = p.client.rpc("remote.list_dirs", json!({ "host": bn, "path": "/" })).await.unwrap_err();
    assert!(format!("{err:#}").contains("home folder"), "{err:#}");

    // A session started for the phone runs on B, and every computer knows it.
    let proj = b.dir.join("proj");
    repo(&proj);
    let cwd = proj.to_string_lossy().into_owned();
    let s = p
        .client
        .rpc("remote.create_code_session", json!({ "host": bn, "cwd": cwd, "agent": "grok" }))
        .await
        .unwrap();
    let id = s["id"].as_str().unwrap().to_owned();
    assert_eq!(s["hostNode"], bn.as_str());
    until("session reaches A", 10, || a.state.db.get_code_session(&id).unwrap().is_some()).await;
    let idx = p.client.rpc("remote.index", Value::Null).await.unwrap();
    assert!(idx["codeSessions"].as_array().unwrap().iter().any(|c| c["id"] == id.as_str()));
    // And the next start offers that folder as recent.
    let opts = p.client.rpc("remote.new_session_options", json!({ "host": bn })).await.unwrap();
    assert_eq!(opts["recent"][0], cwd.as_str());

    // The phone switches it to another agent mid-session; B folds the transcript and seeds the note.
    b.state.db.set_code_transcript(&id, r#"{"v":1,"entries":[{"kind":"user","text":"fix it"}],"toolCalls":{}}"#).unwrap();
    let p_switch = json!({ "method": "code.set_agent", "params": { "id": id, "agent": "claude-code" } });
    p.client.rpc("remote.code", p_switch).await.unwrap();
    assert_eq!(b.state.db.get_code_session(&id).unwrap().unwrap().agent, "claude-code");
    let note = b.state.db.get_setting(&format!("mesh.handoff.{id}")).unwrap().unwrap();
    assert!(note.contains("from another agent, Grok Build") && note.contains("User: fix it"), "{note}");

    // A folder that isn't there is refused; so is a computer that's gone.
    let err = p
        .client
        .rpc("remote.create_code_session", json!({ "host": bn, "cwd": "/nope/missing" }))
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("not a directory"), "{err:#}");
    b.state.mesh.stop();
    until("B gone from A", 15, || !a.state.mesh.connected(&bn)).await;
    let err = p.client.rpc("remote.new_session_options", json!({ "host": bn })).await.unwrap_err();
    assert!(format!("{err:#}").contains("offline"), "{err:#}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn phone_chat_runs_on_the_picked_computer() {
    let (a, b) = (node("alpha5"), node("bravo5"));
    link(&a, &b).await;
    // A provider that refuses connections: the turn starts on B, then fails there.
    let db = &b.state.db;
    let prov = db::Provider {
        id: "dead".into(),
        name: "Dead".into(),
        kind: db::ProviderKind::OpenaiChat,
        base_url: "http://127.0.0.1:9/v1".into(),
        has_key: true,
            auth_mode: Default::default(),
            claude_config_dir: None,
        };
    db.upsert_provider(&prov, Some("k")).unwrap();
    let model = db::Model {
        id: "m".into(),
        provider_id: "dead".into(),
        model_id: "m".into(),
        display_name: "m".into(),
        vision: false,
        context_window: None,
        params: Default::default(),
    };
    db.upsert_model(&model).unwrap();

    let p = phone();
    let url = format!("127.0.0.1:{}", a.state.mesh.bound_port().unwrap());
    p.client.join(&url, PASS, "pocket").unwrap();
    let pid = p.client.node_id();
    until("phone's join request", 10, || {
        a.state.mesh.status()["pending"].as_array().unwrap().iter().any(|r| r["nodeId"] == pid)
    })
    .await;
    a.state.mesh.approve(&pid).await.unwrap();
    until("phone online", 10, || p.client.status()["phase"] == "online").await;
    let bn = b.state.mesh.node_id().to_owned();

    // A new chat, made on the gateway and sent at once: B waits for it to sync, then runs it.
    let conv = p.client.rpc("remote.create_conversation", json!({ "modelRef": "m" })).await.unwrap();
    let cid = conv["id"].as_str().unwrap().to_owned();
    p.client.subscribe(&patchbay_mesh::remote::chat_topic(&cid), true).await.unwrap();
    let send = json!({ "conversationId": cid, "modelRef": "m", "text": "hello", "node": bn });
    p.client.rpc("remote.chat_send", send).await.unwrap();
    until("B streams the reply to the phone", 15, || {
        p.count(|n, e| n == patchbay_mesh::remote::EV_CHAT && e["streamer"] == bn.as_str() && e["event"]["type"] == "started") == 1
    })
    .await;
    until("B's window credits the phone", 10, || {
        b.events.lock().unwrap().iter().any(|(n, e)| {
            n == patchbay_mesh::remote::EV_CHAT && e["streamer"] == pid.as_str() && e["conversationId"] == cid.as_str()
        })
    })
    .await;
    assert!(!a.events.lock().unwrap().iter().any(|(n, e)| n == patchbay_mesh::remote::EV_CHAT && e["streamer"] == pid.as_str()));
    until("the message syncs to A", 10, || a.state.db.list_messages(&cid).unwrap().iter().any(|m| m.content == "hello")).await;

    // A computer that's gone is refused.
    b.state.mesh.stop();
    until("B gone from A", 15, || !a.state.mesh.connected(&bn)).await;
    let send = json!({ "conversationId": cid, "modelRef": "m", "text": "again", "node": bn });
    let err = p.client.rpc("remote.chat_send", send).await.unwrap_err();
    assert!(format!("{err:#}").contains("offline"), "{err:#}");
}

/// A gateway for trying the phone app by hand (an emulator reaches the host at 10.0.2.2):
/// a mesh on `0.0.0.0:41890` whose passphrase is `PASS`, with a provider pointing at
/// `scripts/mock-llm.py` (start it first) and a Patchbay agent session using it. Every phone that
/// asks to join is approved. Each run is a new mesh.
///
/// `cargo test --lib manual_phone_gateway -- --ignored --nocapture` (PATCHBAY_TEST_SECS limits it).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn manual_phone_gateway() {
    let n = node("studio");
    n.state.mesh.set_config(Some("studio"), Some("0.0.0.0:41890"), None).unwrap();
    n.state.mesh.create(PASS, "studio").unwrap();
    let db = &n.state.db;
    let p = db::Provider {
        id: "mock".into(),
        name: "Mock LLM".into(),
        kind: db::ProviderKind::OpenaiChat,
        base_url: std::env::var("PATCHBAY_MOCK_URL").unwrap_or_else(|_| "http://127.0.0.1:8765/v1".into()),
        has_key: true,
            auth_mode: Default::default(),
            claude_config_dir: None,
        };
    db.upsert_provider(&p, Some("mock-key")).unwrap();
    for (id, model) in [("m-echo", "mock-echo"), ("m-agent", "mock-agent")] {
        db.upsert_model(&db::Model {
            id: id.into(),
            provider_id: "mock".into(),
            model_id: model.into(),
            display_name: model.into(),
            vision: true,
            context_window: Some(128_000),
            params: Default::default(),
        })
        .unwrap();
    }
    db.set_setting("default_model", "m-echo").unwrap();
    let proj = n.dir.join("proj");
    repo(&proj);
    let s = db.create_code_session(&proj.to_string_lossy(), Some("m-agent"), "patchbay", "default").unwrap();
    db.update_code_session(&s.id, None, Some("Try !edit from the phone"), None).unwrap();
    let c = db.create_conversation(Some("m-echo")).unwrap();
    db.update_conversation(&c.id, Some("Hello from studio"), None, None).unwrap();
    let u = db.insert_message(&c.id, None, "user", "Is this thing on?", None, &[]).unwrap();
    let a = db.insert_message(&c.id, Some(&u), "assistant", "Loud and clear.", Some("m-echo"), &[]).unwrap();
    db.set_active_leaf(&c.id, &a).unwrap();
    serve_phones(&n).await;
}

async fn serve_phones(n: &Node) {
    eprintln!("gateway: ws://0.0.0.0:41890, passphrase {PASS:?}, node {}", n.state.mesh.node_id());
    let secs: u64 = std::env::var("PATCHBAY_TEST_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(900);
    let t0 = std::time::Instant::now();
    let mut last = String::new();
    while t0.elapsed() < Duration::from_secs(secs) {
        let st = n.state.mesh.status();
        for p in st["pending"].as_array().cloned().unwrap_or_default() {
            let id = p["nodeId"].as_str().unwrap_or_default();
            eprintln!("approving {} ({}), code {}", p["name"], p["role"], p["code"]);
            let _ = n.state.mesh.approve(id).await;
        }
        let members: Vec<String> =
            st["members"].as_array().unwrap().iter().map(|m| format!("{}:{}", m["name"], m["state"])).collect();
        let now = members.join(" ");
        if now != last {
            eprintln!("members: {now}");
            last = now;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

/// The window's `code_authenticate` command, and an `authRequired` event kept for windows that
/// attach while the agent waits for a sign-in.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sign_in_command_and_pending_auth() {
    use code::acp::{AuthChoice, CodeEvent};
    let a = node("alpha-auth");
    let s = a.state.db.create_code_session(&a.dir.to_string_lossy(), None, "codex", "default").unwrap();
    let chans: crate::api::ChanFactory = Arc::new(|_| None);
    let call = |args: Value| crate::api::call(&a.state, "code_authenticate", args, &chans);
    let err = call(json!({ "id": s.id })).await.unwrap_err();
    assert!(err.0.contains("methodId"), "{}", err.0);
    let err = call(json!({ "id": s.id, "methodId": "chat-gpt" })).await.unwrap_err();
    assert!(err.0.contains("agent is not running"), "{}", err.0);

    let auth = || a.state.live.lock().unwrap().get(&s.id).and_then(|l| l.auth.clone());
    let methods = vec![AuthChoice { id: "chat-gpt".into(), name: "ChatGPT".into(), description: None, api_key: false, terminal: false }];
    emit(&a, &s.id, CodeEvent::AuthRequired { message: "Codex needs you to sign in first.".into(), methods });
    let ev = auth().expect("kept for late viewers");
    assert_eq!(ev["type"], "authRequired");
    assert_eq!(ev["methods"][0]["id"], "chat-gpt");
    // A sign-in mid-session clears it without a new `sessionReady`.
    emit(&a, &s.id, CodeEvent::AuthResolved);
    assert!(auth().is_none());
    emit(&a, &s.id, CodeEvent::AuthRequired { message: "again".into(), methods: Vec::new() });
    assert!(auth().is_some());
    assert!(a.events.lock().unwrap().iter().any(|(n, p)| n == code::acp::EVENT && p["event"]["type"] == "authRequired"));
    emit(&a, &s.id, CodeEvent::SessionReady {
        acp_session_id: "acp".into(),
        models: Value::Null,
        modes: Value::Null,
        loaded: false,
        capabilities: Value::Null,
        agent_info: Value::Null,
        config_options: json!([]),
        extensions: Default::default(),
        approval_gaps: &[],
    });
    assert!(auth().is_none(), "signed in: nothing left to replay");
}

/// A window (or phone) that attaches to a running session gets the agent's latest plan, even
/// when it was written turns ago.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn attach_replays_latest_plan() {
    use code::acp::CodeEvent;
    let a = node("alpha-plan");
    let s = a.state.db.create_code_session(&a.dir.to_string_lossy(), None, "patchbay", "default").unwrap();
    let _rx = a.state.code.attach_test_agent(&s.id);
    let plan = |step: &str| CodeEvent::Update {
        update: json!({ "sessionUpdate": "plan", "entries": [{ "content": step, "status": "pending", "priority": "medium" }] }),
    };
    let plans = || -> Vec<Value> {
        let events = code_attach_local(&a.state, &s.id)["events"].as_array().cloned().unwrap_or_default();
        events.into_iter().filter(|e| e["update"]["sessionUpdate"] == "plan").collect()
    };
    assert!(plans().is_empty(), "no plan yet");
    emit(&a, &s.id, plan("map the installer"));
    emit(&a, &s.id, plan("decode the archive"));
    emit(&a, &s.id, chunk("working"));
    let got = plans();
    assert_eq!(got.len(), 1, "only the latest plan is kept");
    assert_eq!(got[0]["update"]["entries"][0]["content"], "decode the archive");
}

/// pb-20261107-17: a turn with nothing from its agent past the limit is reported once (a
/// `stalled` event in its transcript, replayed to a window that opens it, and `stalled` in
/// `list_agent_sessions`), never cancelled, and cleared by the agent's next word.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_quiet_turn_is_reported_stalled_once_and_cleared_by_activity() {
    use code::acp::CodeEvent;
    let a = node("alpha-stall");
    let s = a.state.db.create_code_session(&a.dir.to_string_lossy(), None, "opencode", "default").unwrap();
    let mut rx = a.state.code.attach_test_agent(&s.id);
    let stalls = || {
        a.events.lock().unwrap().iter().filter(|(n, p)| n == code::acp::EVENT && p["event"]["type"] == "stalled").count()
    };
    let activity = || {
        let listed = list_agent_sessions(&a.state, &s.id).unwrap();
        let me = listed.as_array().unwrap().iter().find(|x| x["sessionId"] == s.id.as_str()).cloned().unwrap();
        (me["activity"].as_str().unwrap_or_default().to_owned(), me["lastActivityAt"].clone())
    };
    let replayed = || {
        code_attach_local(&a.state, &s.id)["events"].as_array().unwrap().iter().any(|e| e["type"] == "stalled")
    };
    assert_eq!(activity().0, "idle");

    // Mid-turn, and the agent's last word was eleven minutes ago.
    a.state.code.set_busy(&s.id, true);
    emit(&a, &s.id, chunk("merging now"));
    assert_eq!(activity().0, "running");
    assert!(activity().1.is_i64(), "last activity is reported");
    let ago = std::time::Instant::now().checked_sub(std::time::Duration::from_secs(11 * 60)).unwrap();
    a.state.code.stall.touch_at(&s.id, ago, 1);
    check_stalls(&a.state);
    assert_eq!(stalls(), 1);
    assert_eq!(activity().0, "stalled");
    assert!(replayed(), "a window opening it now sees why");
    check_stalls(&a.state);
    assert_eq!(stalls(), 1, "reported once");
    assert!(rx.try_recv().is_err(), "nothing was sent to the agent: no cancel");
    assert!(a.state.code.is_busy(&s.id), "the turn is left running");

    // The agent speaks again: no longer stalled, and nothing to replay.
    emit(&a, &s.id, chunk("done"));
    assert_eq!(activity().0, "running");
    assert!(!replayed());

    // Waiting on a person is never a stall.
    a.state.code.stall.touch_at(&s.id, ago, 1);
    emit(&a, &s.id, CodeEvent::Question { request_id: "q1".into(), form: json!({}) });
    a.state.code.stall.touch_at(&s.id, ago, 1);
    check_stalls(&a.state);
    assert_eq!(stalls(), 1);
    assert_eq!(activity().0, "waiting");

    // Off at 0.
    emit(&a, &s.id, CodeEvent::QuestionResolved { request_id: "q1".into() });
    let mut p = prefs::load(&a.state.db);
    p.stall_notice_minutes = 0;
    prefs::save(&a.state.db, &p).unwrap();
    a.state.code.stall.touch_at(&s.id, ago, 1);
    check_stalls(&a.state);
    assert_eq!(stalls(), 1, "off");
}

static TRACING_LINES: Mutex<Vec<String>> = Mutex::new(Vec::new());

struct TestLogWriter;

impl std::io::Write for TestLogWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let s = String::from_utf8_lossy(buf).into_owned();
        eprint!("{s}");
        TRACING_LINES.lock().unwrap().push(s);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn ensure_test_tracing() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        let _ = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_max_level(tracing::Level::INFO)
            .with_writer(|| TestLogWriter)
            .try_init();
    });
}

fn unidentified_default_logs_for(node_id: &str) -> Vec<String> {
    crate::UNIDENTIFIED_DEFAULT_PLAN_LOGS
        .lock()
        .unwrap()
        .iter()
        .filter(|(n, _)| n == node_id)
        .map(|(_, msg)| msg.clone())
        .collect()
}

struct TempClaudeEnv {
    _guard: tokio::sync::MutexGuard<'static, ()>,
    dir: PathBuf,
}

impl TempClaudeEnv {
    async fn new(portable_default: Option<(&str, &str, &str)>, config_dir: Option<String>) -> Self {
        ensure_test_tracing();
        let guard = claude_auth::ENV_LOCK.lock().await;
        let dir = std::env::temp_dir().join(format!("pb-claude-home-{}", db::new_id()));
        std::fs::create_dir_all(dir.join(".claude")).unwrap();
        if let Some((u, o, name)) = portable_default {
            let profile = serde_json::json!({ "oauthAccount": {
                "accountUuid": u, "organizationUuid": o, "organizationName": name
            }});
            std::fs::write(dir.join(".claude.json"), profile.to_string()).unwrap();
        }
        claude_auth::set_test_env(Some(dir.clone()), config_dir);
        Self { _guard: guard, dir }
    }

    fn new_blocking(portable_default: Option<(&str, &str, &str)>, config_dir: Option<String>) -> Self {
        ensure_test_tracing();
        let guard = claude_auth::ENV_LOCK.blocking_lock();
        let dir = std::env::temp_dir().join(format!("pb-claude-home-{}", db::new_id()));
        std::fs::create_dir_all(dir.join(".claude")).unwrap();
        if let Some((u, o, name)) = portable_default {
            let profile = serde_json::json!({ "oauthAccount": {
                "accountUuid": u, "organizationUuid": o, "organizationName": name
            }});
            std::fs::write(dir.join(".claude.json"), profile.to_string()).unwrap();
        }
        claude_auth::set_test_env(Some(dir.clone()), config_dir);
        Self { _guard: guard, dir }
    }
}

impl Drop for TempClaudeEnv {
    fn drop(&mut self) {
        claude_auth::set_test_env(None, None);
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// The usage guard: a Claude Code turn past the plan limit is cancelled once, with a notice, and
/// new prompts are refused until the window resets. Sessions on other agents run on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn usage_limit_stops_claude_turns() {
    use code::acp::{AgentCmd, CodeEvent, Delivery};
    use code::plan_limits::PlanLimits;
    let _env = TempClaudeEnv::new(Some(("u-limit", "o-limit", "LimitOrg")), None).await;
    let a = node("alpha-limit");
    let cwd = a.dir.to_string_lossy().into_owned();
    let claude = a.state.db.create_code_session(&cwd, None, "claude-code", "default").unwrap();
    let grok = a.state.db.create_code_session(&cwd, None, "grok", "default").unwrap();
    let mut claude_rx = a.state.code.attach_test_agent(&claude.id);
    let mut grok_rx = a.state.code.attach_test_agent(&grok.id);
    for s in [&claude.id, &grok.id] {
        emit(&a, s, CodeEvent::UserPrompt {
            prompt_id: db::new_id(),
            text: "go".into(),
            origin: String::new(),
            attachments: json!([]),
            delivery: Delivery::Sent,
        });
    }
    let resets = code::plan_limits::now() + 3600;
    let limits = |five: f64| {
        PlanLimits::from_update(&json!({ "_meta": { "_claude/rateLimit": { "status": "allowed", "unifiedWindows": {
            "five_hour": { "utilization": five, "resetsAt": resets },
            "seven_day": { "utilization": 0.3, "resetsAt": resets + 86400 }
        }}}}))
        .unwrap()
    };
    let notices = || {
        a.events.lock().unwrap().iter().filter(|(n, p)| n == code::acp::EVENT && p["event"]["type"] == "notice").count()
    };
    let chans: crate::api::ChanFactory = Arc::new(|_| None);
    let prompt = |id: String| crate::api::call(&a.state, "code_prompt", json!({ "id": id, "text": "more", "attachmentIds": [] }), &chans);

    emit(&a, &claude.id, CodeEvent::PlanLimits { limits: limits(0.9) });
    assert!(claude_rx.try_recv().is_err(), "under the limit");

    emit(&a, &claude.id, CodeEvent::PlanLimits { limits: limits(0.96) });
    assert!(matches!(claude_rx.try_recv(), Ok(AgentCmd::Cancel)));
    assert!(grok_rx.try_recv().is_err(), "other agents have no plan limit");
    assert_eq!(notices(), 1);
    // Later reports in the same turn don't stop it again.
    emit(&a, &claude.id, CodeEvent::PlanLimits { limits: limits(0.97) });
    assert!(claude_rx.try_recv().is_err());
    assert_eq!(notices(), 1);
    let ev = a.events.lock().unwrap().iter().rev().find(|(_, p)| p["event"]["type"] == "notice").cloned().unwrap();
    let text = ev.1["event"]["message"].as_str().unwrap().to_owned();
    let label = crate::plan_account_label(&a.state, &crate::default_plan_account().0);
    assert!(text.starts_with(&format!("Stopped. You have used 96% of {label}'s 5-hour limit")), "{text}");

    // A session on another Claude account runs on: that account's usage is its own.
    let teams = db::Provider {
        id: "teams".into(),
        name: "Claude (Teams)".into(),
        kind: db::ProviderKind::Anthropic,
        base_url: "https://api.anthropic.com/v1".into(),
        has_key: false,
        auth_mode: db::AuthMode::ClaudeLogin,
        claude_config_dir: Some("/tmp/pb-other-claude".into()),
    };
    a.state.db.upsert_provider(&teams, None).unwrap();
    let model = db::Model {
        id: "tm".into(),
        provider_id: "teams".into(),
        model_id: "claude-x".into(),
        display_name: "x".into(),
        vision: false,
        context_window: None,
        params: Default::default(),
    };
    a.state.db.upsert_model(&model).unwrap();
    let other = a.state.db.create_code_session(&cwd, Some("tm"), "patchbay", "default").unwrap();
    let mut other_rx = a.state.code.attach_test_agent(&other.id);
    emit(&a, &other.id, CodeEvent::UserPrompt {
        prompt_id: db::new_id(),
        text: "go".into(),
        origin: String::new(),
        attachments: json!([]),
        delivery: Delivery::Sent,
    });
    emit(&a, &other.id, CodeEvent::PlanLimits { limits: limits(0.2) });
    assert!(other_rx.try_recv().is_err(), "the other account is under its limit");
    assert_eq!(notices(), 1);
    let usable = a.state.code.plan.hit("/tmp/pb-other-claude", &prefs::load(&a.state.db).usage_limit, code::plan_limits::now());
    assert!(usable.is_none());

    emit(&a, &claude.id, CodeEvent::PromptDone { stop_reason: json!("cancelled") });
    let err = prompt(claude.id.clone()).await.unwrap_err();
    assert!(err.0.starts_with("Not sent. You have used 97%"), "{}", err.0);

    // Turning the guard off lets prompts through again.
    let mut prefs = prefs::load(&a.state.db);
    prefs.usage_limit.enabled = false;
    crate::api::call(&a.state, "set_preferences", json!({ "prefs": prefs }), &chans).await.unwrap();
    assert!(a.state.code.plan.hit(&crate::default_plan_account().0, &prefs::load(&a.state.db).usage_limit, code::plan_limits::now()).is_none());

    // Lowering the limit below current usage stops a running turn right away.
    emit(&a, &claude.id, CodeEvent::UserPrompt {
        prompt_id: db::new_id(),
        text: "go".into(),
        origin: String::new(),
        attachments: json!([]),
        delivery: Delivery::Sent,
    });
    prefs.usage_limit = prefs::UsageLimit { enabled: true, percent: 50, ..Default::default() };
    crate::api::call(&a.state, "set_preferences", json!({ "prefs": prefs }), &chans).await.unwrap();
    // Other windows on this backend hear about the change.
    assert!(
        a.events.lock().unwrap().iter().any(|(n, p)| n == crate::PREFS_CHANGED_EVENT && p["usageLimit"]["percent"] == 50)
    );
    assert!(matches!(claude_rx.try_recv(), Ok(AgentCmd::Cancel)));
    assert_eq!(notices(), 2);

    // The panel reads each account's last report, named, and a restart keeps them.
    let v = crate::api::call(&a.state, "plan_accounts", json!({}), &chans).await.unwrap();
    let by = |label: &str| v.as_array().unwrap().iter().find(|x| x["label"] == label).cloned().unwrap();
    assert_eq!(by("Claude Code login")["latest"]["fiveHour"]["utilization"], 0.97);
    assert_eq!(by("Claude (Teams)")["latest"]["fiveHour"]["utilization"], 0.2);
    assert!(a.events.lock().unwrap().iter().any(|(n, p)| n == "code://plan-limits" && p[0]["latest"]["sevenDay"]["utilization"] == 0.3));
    let again = AppState::new(a.state.db.clone(), a.dir.clone(), a.state.mesh.clone(), Arc::new(|_: &str, _: Value| {}));
    let latest = |acct: &str| again.code.plan.account(acct).latest.and_then(|l| l.five_hour).map(|w| w.utilization);
    assert_eq!(latest(&crate::default_plan_account().0), Some(0.97));
    assert_eq!(latest("/tmp/pb-other-claude"), Some(0.2));
}

/// Two accounts in one guard, constructed directly: a session is stopped only by its own
/// account's report — never by another account's 99%, and never at all when its own account
/// has no report. This is the contract `PlanGuard` states ("Accounts never count against
/// each other"), pinned at the prompt gate where a violation would actually block a send.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_is_gated_only_by_its_own_accounts_report() {
    use code::acp::{AgentCmd, Delivery};
    use code::plan_limits::{PlanLimits, Window, now};
    let a = node("alpha-own-account");
    let cwd = a.dir.to_string_lossy().into_owned();
    let resets = now() + 3600;
    let observed = now() - 60;
    // Account A over the limit, account B well under it — recorded straight into the guard.
    a.state.code.plan.record(
        "acct-a:org-a",
        Some("Alpha".into()),
        PlanLimits { five_hour: Some(Window { utilization: 0.99, resets_at: resets }), observed_at: observed, ..Default::default() },
    );
    a.state.code.plan.record(
        "acct-b:org-b",
        Some("Beta".into()),
        PlanLimits { five_hour: Some(Window { utilization: 0.05, resets_at: resets }), observed_at: observed, ..Default::default() },
    );
    // One provider per account: each config directory's `.claude.json` names its account, so
    // sessions bound to a provider resolve to exactly that key (and account C has no report).
    let provider = |id: &str, name: &str, account: (&str, &str)| {
        let dir = a.dir.join(id);
        std::fs::create_dir_all(&dir).unwrap();
        let profile = serde_json::json!({ "oauthAccount": {
            "accountUuid": account.0, "organizationUuid": account.1, "organizationName": name
        }});
        std::fs::write(dir.join(".claude.json"), profile.to_string()).unwrap();
        a.state
            .db
            .upsert_provider(
                &db::Provider {
                    id: id.into(),
                    name: name.into(),
                    kind: db::ProviderKind::Anthropic,
                    base_url: "https://api.anthropic.com/v1".into(),
                    has_key: false,
                    auth_mode: db::AuthMode::ClaudeLogin,
                    claude_config_dir: Some(dir.to_string_lossy().into_owned()),
                },
                None,
            )
            .unwrap();
        let model = db::Model {
            id: format!("m-{id}"),
            provider_id: id.into(),
            model_id: "claude-x".into(),
            display_name: name.into(),
            vision: false,
            context_window: None,
            params: Default::default(),
        };
        a.state.db.upsert_model(&model).unwrap();
        model.id
    };
    // A session on a provider, with a test agent that answers its prompts.
    let on = |model: &str| {
        let s = a.state.db.create_code_session(&cwd, Some(model), "patchbay", "default").unwrap();
        let mut rx = a.state.code.attach_test_agent(&s.id);
        tokio::spawn(async move {
            while let Some(cmd) = rx.recv().await {
                if let AgentCmd::Prompt { reply, .. } = cmd
                    && let Some(reply) = reply
                {
                    let _ = reply.send(Ok(Delivery::Sent));
                }
            }
        });
        s.id
    };
    let model_a = provider("prov-a", "Claude Alpha", ("acct-a", "org-a"));
    let model_b = provider("prov-b", "Claude Beta", ("acct-b", "org-b"));
    let model_c = provider("prov-c", "Claude Gamma", ("acct-c", "org-c"));
    let sa = on(&model_a);
    let sb = on(&model_b);
    let sc = on(&model_c);
    let chans: crate::api::ChanFactory = Arc::new(|_| None);
    let prompt = |id: String| crate::api::call(&a.state, "code_prompt", json!({ "id": id, "text": "more", "attachmentIds": [] }), &chans);

    // A reads A's own 99% — and the message names A's account, not a generic "your Claude plan".
    let err = prompt(sa).await.unwrap_err();
    assert!(err.0.starts_with("Not sent. You have used 99% of Claude Alpha's 5-hour limit"), "{}", err.0);
    // B reads B's 5%: another account's 99% never stops this session.
    prompt(sb).await.expect("B is not gated by A's numbers");
    // C has no report of its own: no report, no block — never another account's numbers.
    prompt(sc).await.expect("an account with no report is not blocked by another's");
}

/// Plan usage one member sees reaches the others, a member that was away catches up when it
/// reconnects, and an older report never replaces a newer one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn plan_limits_reach_every_member() {
    use code::acp::CodeEvent;
    use code::plan_limits::{PlanLimits, Window, now};
    let _env = TempClaudeEnv::new(Some(("u-mesh-def", "o-mesh-def", "MeshDefault")), None).await;
    let (a, b) = (node("alpha-plan"), node("bravo-plan"));
    link(&a, &b).await;
    let bn = b.state.mesh.node_id().to_owned();
    let report = |five: f64, ago: i64| PlanLimits {
        five_hour: Some(Window { utilization: five, resets_at: now() + 3600 }),
        observed_at: now() - ago,
        ..Default::default()
    };
    // Sessions the nodes don't know count as the default login's, pinned to the temp HOME fixture here.
    let (me, _) = crate::default_plan_account();
    assert_eq!(me, "u-mesh-def:o-mesh-def");
    let five_of = |n: &Node, acct: &str| n.state.code.plan.account(acct).latest.and_then(|l| l.five_hour).map(|w| w.utilization);
    let five = |n: &Node| five_of(n, &me);

    emit(&a, "s", CodeEvent::PlanLimits { limits: report(0.4, 60) });
    until("B has A's report", 10, || five(&b) == Some(0.4)).await;
    // The panel event follows the record, so wait for it too.
    until("B's panel hears of it", 10, || {
        b.events.lock().unwrap().iter().any(|(n, p)| n == "code://plan-limits" && p[0]["latest"]["fiveHour"]["utilization"] == 0.4)
    })
    .await;
    assert!(b.state.db.get_setting("claude.planByAccount").unwrap().is_some());

    b.state.mesh.stop();
    until("A notices B is gone", 40, || !a.state.mesh.connected(&bn)).await;
    emit(&a, "s", CodeEvent::PlanLimits { limits: report(0.5, 30) });
    b.state.mesh.start();
    until("B catches up on reconnect", 30, || five(&b) == Some(0.5)).await;
    // B's older report, sent to A on reconnect, didn't replace A's.
    assert_eq!(five(&a), Some(0.5));

    // An untagged report, from a member before accounts were told apart, is the default login's.
    mesh::RpcHandler::event(&MeshRpc { state: b.state.clone() }, MESH_PLAN_LIMITS_EVENT, &json!(report(0.1, 120)));
    assert_eq!(five(&b), Some(0.5));
    // Another account's report is kept apart, under its own name.
    let other = json!({ "account": "u2:o2", "name": "Team", "limits": report(0.9, 10) });
    mesh::RpcHandler::event(&MeshRpc { state: b.state.clone() }, MESH_PLAN_LIMITS_EVENT, &other);
    assert_eq!((five(&b), five_of(&b, "u2:o2")), (Some(0.5), Some(0.9)));
    assert_eq!(b.state.code.plan.account("u2:o2").name.as_deref(), Some("Team"));

    // A path-keyed report from another member (Claude Code had no oauthAccount there) is the
    // same login as the default here — not a second account under that foreign path.
    let foreign = json!({ "account": "/home/molly/.claude", "limits": report(0.53, 5) });
    mesh::RpcHandler::event(&MeshRpc { state: b.state.clone() }, MESH_PLAN_LIMITS_EVENT, &foreign);
    assert_eq!(five(&b), Some(0.53));
    assert!(
        !b.state.code.plan.accounts().contains_key("/home/molly/.claude"),
        "foreign path fallbacks must not appear as their own account"
    );
    let foreign_work = json!({ "account": "/Users/other/work", "limits": report(0.55, 2) });
    mesh::RpcHandler::event(&MeshRpc { state: b.state.clone() }, MESH_PLAN_LIMITS_EVENT, &foreign_work);
    assert_eq!(five(&b), Some(0.55));
    assert!(
        !b.state.code.plan.accounts().contains_key("/Users/other/work"),
        "foreign non-default path must collapse onto default, not stay as a ghost account"
    );
}

/// Saved path keys from another computer collapse into the default login on load.
#[test]
fn load_folds_foreign_path_accounts_into_the_default() {
    use code::plan_limits::{Account, PlanLimits, Window};
    let _env = TempClaudeEnv::new_blocking(Some(("u-fold-def", "o-fold-def", "FoldDefault")), None);
    let n = node("fold-path-load");
    let (me, name) = crate::default_plan_account();
    let mut raw = std::collections::BTreeMap::new();
    raw.insert(
        me.clone(),
        Account {
            name: name.clone(),
            latest: Some(PlanLimits {
                five_hour: Some(Window { utilization: 0.9, resets_at: code::plan_limits::now() + 3600 }),
                seven_day: Some(Window { utilization: 0.9, resets_at: code::plan_limits::now() + 86400 }),
                observed_at: code::plan_limits::now(),
                ..Default::default()
            }),
            history: vec![],
        },
    );
    raw.insert(
        "/home/molly/.claude".into(),
        Account {
            name: None,
            latest: Some(PlanLimits {
                five_hour: Some(Window { utilization: 0.12, resets_at: code::plan_limits::now() + 3600 }),
                seven_day: Some(Window { utilization: 0.53, resets_at: code::plan_limits::now() + 86400 }),
                observed_at: code::plan_limits::now() - 2 * 86400,
                ..Default::default()
            }),
            history: vec![],
        },
    );
    raw.insert(
        "/Users/other/work".into(),
        Account {
            name: None,
            latest: Some(PlanLimits {
                five_hour: Some(Window { utilization: 0.34, resets_at: code::plan_limits::now() + 3600 }),
                seven_day: Some(Window { utilization: 0.64, resets_at: code::plan_limits::now() + 86400 }),
                observed_at: code::plan_limits::now() - 86400,
                ..Default::default()
            }),
            history: vec![],
        },
    );
    n.state.db.set_setting("claude.planByAccount", &serde_json::to_string(&raw).unwrap()).unwrap();
    let loaded = crate::load_plan_accounts(&n.state.db);
    assert_eq!(loaded.len(), 1, "{loaded:?}");
    assert!(loaded.contains_key(&me));
    assert!(!loaded.contains_key("/home/molly/.claude"));
    assert!(!loaded.contains_key("/Users/other/work"));
    assert_eq!(loaded[&me].latest.as_ref().unwrap().seven_day.unwrap().utilization, 0.9);
}

/// A `claude-code` session bound to its own `claude.config_dir.<id>` (even one ending in
/// `/.claude`, like `<dir>/work/.claude`) reports and is gated under that directory's account
/// (not `default_plan_account`), and when that directory upgrades from a path fallback to a
/// portable `u:o` key at runtime, both `adopt_plan_limits` and `plan_views` / `plan_accounts`
/// fold the path-keyed history into the portable key without a restart.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_path_upgrade_and_session_config_dir_keep_accounts_apart() {
    use code::acp::CodeEvent;
    use code::plan_limits::{PlanLimits, Window, now};
    let _env = TempClaudeEnv::new(Some(("u-def-live", "o-def-live", "DefaultLive")), None).await;
    let n = node("live-fold-and-session-dir");
    let cwd = n.dir.to_string_lossy().into_owned();
    let custom_dir = n.dir.join("work").join(".claude");
    std::fs::create_dir_all(&custom_dir).unwrap();
    let custom_path = custom_dir.to_string_lossy().into_owned();

    // Mutant (i): a foreign non-default path must collapse onto default in `resolve_plan_account`.
    assert_eq!(
        crate::resolve_plan_account(&n.state.db, "/Users/other/work", None),
        ("u-def-live:o-def-live".to_owned(), Some("DefaultLive".to_owned())),
        "foreign path /Users/other/work must collapse onto default in resolve_plan_account"
    );

    let s = n.state.db.create_code_session(&cwd, None, "claude-code", "default").unwrap();
    n.state
        .db
        .set_setting(&code::import_claude::config_dir_key(&s.id), &custom_path)
        .unwrap();

    // Mutant (k): `work/.claude` is a local custom dir and must resolve to `custom_path`, not default.
    assert_eq!(
        crate::resolve_plan_account(&n.state.db, &custom_path, None),
        (custom_path.clone(), None),
        "custom dir ending in /.claude must resolve to the custom dir, not default"
    );

    // First report arrives before `.claude.json` exists in `custom_dir`: stored under `custom_path`,
    // never under `default_plan_account()` (even though `custom_path` ends with `/.claude`).
    let first = PlanLimits {
        five_hour: Some(Window { utilization: 0.15, resets_at: now() + 3600 }),
        seven_day: Some(Window { utilization: 0.60, resets_at: now() + 86400 }),
        observed_at: now() - 60,
        ..Default::default()
    };
    emit(&n, &s.id, CodeEvent::PlanLimits { limits: first });
    assert!(
        n.state.code.plan.accounts().contains_key(&custom_path),
        "session with claude.config_dir.<id> must record under its own directory, not default"
    );
    assert!(
        !n.state.code.plan.accounts().contains_key(&crate::default_plan_account().0),
        "default account must stay untouched even when custom config dir ends in /.claude"
    );

    // Now Claude Code writes `.claude.json` in `custom_dir`. Sending a report folds `custom_path`
    // (and its 0.15 sample) into `u-team:o-team` directly inside `adopt_plan_limits`, WITHOUT
    // calling `plan_accounts`.
    let profile = serde_json::json!({ "oauthAccount": {
        "accountUuid": "u-team", "organizationUuid": "o-team", "organizationName": "Vizius"
    }});
    std::fs::write(custom_dir.join(".claude.json"), profile.to_string()).unwrap();
    let second = PlanLimits {
        five_hour: Some(Window { utilization: 0.25, resets_at: now() + 3600 }),
        seven_day: Some(Window { utilization: 0.61, resets_at: now() + 86400 }),
        observed_at: now() - 30,
        ..Default::default()
    };
    emit(&n, &s.id, CodeEvent::PlanLimits { limits: second });
    assert!(
        !n.state.code.plan.accounts().contains_key(&custom_path),
        "adopt_plan_limits must fold the upgraded path key out of plan.accounts() without calling plan_accounts"
    );
    let team = n.state.code.plan.account("u-team:o-team");
    assert_eq!(team.latest.unwrap().five_hour.unwrap().utilization, 0.25);
    assert_eq!(
        team.history.len(),
        2,
        "adopt_plan_limits must merge the pre-upgrade path-key history into u-team:o-team: {:?}",
        team.history
    );

    // Mutant (h) + Item 6: a second custom dir records a path-keyed report, upgrades via
    // `.claude.json`, and is read through `session_plan_usage_local` and `plan_accounts` WITHOUT
    // any subsequent `adopt_plan_limits` call — `plan_views` must fold it into `accounts()`.
    let custom_dir2 = n.dir.join("work-two").join(".claude");
    std::fs::create_dir_all(&custom_dir2).unwrap();
    let custom_path2 = custom_dir2.to_string_lossy().into_owned();
    let s2 = n.state.db.create_code_session(&cwd, None, "claude-code", "default").unwrap();
    n.state
        .db
        .set_setting(&code::import_claude::config_dir_key(&s2.id), &custom_path2)
        .unwrap();
    emit(
        &n,
        &s2.id,
        CodeEvent::PlanLimits {
            limits: PlanLimits {
                five_hour: Some(Window { utilization: 0.45, resets_at: now() + 3600 }),
                seven_day: Some(Window { utilization: 0.70, resets_at: now() + 86400 }),
                observed_at: now() - 20,
                ..Default::default()
            },
        },
    );
    assert!(n.state.code.plan.accounts().contains_key(&custom_path2));
    std::fs::write(
        custom_dir2.join(".claude.json"),
        serde_json::json!({ "oauthAccount": {
            "accountUuid": "u-two", "organizationUuid": "o-two", "organizationName": "SecondOrg"
        }})
        .to_string(),
    )
    .unwrap();

    let usage_view = serde_json::to_value(crate::session_plan_usage_local(&n.state, &s2.id)).unwrap();
    assert_eq!(usage_view["account"], "u-two:o-two", "{usage_view:?}");
    assert_eq!(usage_view["latest"]["fiveHour"]["utilization"], 0.45, "{usage_view:?}");

    let chans: crate::api::ChanFactory = Arc::new(|_| None);
    let views = crate::api::call(&n.state, "plan_accounts", json!({}), &chans).await.unwrap();
    let arr = views.as_array().unwrap();
    assert!(arr.iter().any(|v| v["account"] == "u-team:o-team" && v["label"] == "Vizius"), "{arr:?}");
    assert!(arr.iter().any(|v| v["account"] == "u-two:o-two" && v["label"] == "SecondOrg"), "{arr:?}");
    assert!(!arr.iter().any(|v| v["account"] == custom_path || v["account"] == custom_path2), "{arr:?}");
    assert!(
        !n.state.code.plan.accounts().contains_key(&custom_path2),
        "plan_views / plan_accounts must fold upgraded path keys in plan.accounts() without a prior adopt_plan_limits call"
    );
    assert_eq!(
        n.state.code.plan.account("u-two:o-two").latest.unwrap().five_hour.unwrap().utilization,
        0.45
    );
}

/// Final rule (`pb-20261109-05`):
/// - Temp `HOME` WITHOUT portable default identity => default-dir report is NOT delivered,
///   secondary path-key report is NOT delivered, `greeting()` carries neither, the sender logs
///   `"plan usage not shared: default account has no portable identity"` once per state change,
///   and a portable control message on the same mesh link IS delivered in the same run.
/// - Temp `HOME` WITH portable default identity => default-dir report IS delivered tagged to peers.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn path_keyed_plan_reports_never_broadcast_or_greet_peers() {
    use code::acp::CodeEvent;
    use code::plan_limits::{PlanLimits, Window, now};
    let env = TempClaudeEnv::new(None, None).await;
    let (a, b) = (node("alpha-no-path-bcast"), node("bravo-no-path-bcast"));
    link(&a, &b).await;
    let a_id = a.state.mesh.node_id().to_owned();

    let cwd = a.dir.to_string_lossy().into_owned();
    let default_path = claude_auth::resolve_dir(None).unwrap().to_string_lossy().into_owned();
    assert!(claude_auth::is_path_account_key(&default_path));

    // 1. Default session on `a` (uses temp HOME's `~/.claude`, which has no `.claude.json`).
    let s_def = a.state.db.create_code_session(&cwd, None, "claude-code", "default").unwrap();

    // 2. Secondary session on `a` with its own `claude.config_dir.<id>` (`<a.dir>/work/.claude`, no `.claude.json`).
    let custom_dir = a.dir.join("work").join(".claude");
    std::fs::create_dir_all(&custom_dir).unwrap();
    let custom_path = custom_dir.to_string_lossy().into_owned();
    let s_path = a.state.db.create_code_session(&cwd, None, "claude-code", "default").unwrap();
    a.state
        .db
        .set_setting(&code::import_claude::config_dir_key(&s_path.id), &custom_path)
        .unwrap();

    // 3. Control session on `a` with a portable `.claude.json` (`u-port:o-port`) to prove the mesh
    //    channel delivers messages in the same run while path-keyed default/secondary reports are withheld.
    let portable_dir = a.dir.join("portable-claude");
    std::fs::create_dir_all(&portable_dir).unwrap();
    std::fs::write(
        portable_dir.join(".claude.json"),
        serde_json::json!({ "oauthAccount": {
            "accountUuid": "u-port", "organizationUuid": "o-port", "organizationName": "PortableOrg"
        }})
        .to_string(),
    )
    .unwrap();
    let s_port = a.state.db.create_code_session(&cwd, None, "claude-code", "default").unwrap();
    a.state
        .db
        .set_setting(
            &code::import_claude::config_dir_key(&s_port.id),
            &portable_dir.to_string_lossy(),
        )
        .unwrap();

    let report = |five: f64, ago: i64| PlanLimits {
        five_hour: Some(Window { utilization: five, resets_at: now() + 3600 }),
        observed_at: now() - ago,
        ..Default::default()
    };

    // Emit TWO unidentified default-dir reports and one unidentified secondary-dir report.
    emit(&a, &s_def.id, CodeEvent::PlanLimits { limits: report(0.41, 50) });
    emit(&a, &s_def.id, CodeEvent::PlanLimits { limits: report(0.42, 45) });
    emit(&a, &s_path.id, CodeEvent::PlanLimits { limits: report(0.33, 40) });
    assert!(
        a.state.code.plan.accounts().contains_key(&default_path),
        "sender still adopts its own unidentified default report locally"
    );
    assert!(
        a.state.code.plan.accounts().contains_key(&custom_path),
        "sender still adopts its own secondary path-keyed report locally"
    );

    // `greeting()` on `a` must not carry any path-keyed plan report (default or secondary).
    let greeting = mesh::RpcHandler::greeting(&MeshRpc { state: a.state.clone() });
    assert!(
        !greeting.iter().any(|(ev, _)| ev == MESH_PLAN_LIMITS_EVENT),
        "greeting() must send nothing when only path-keyed accounts exist: {greeting:?}"
    );

    // Sender-side visibility log line emitted ONCE per state change (not per broadcast/greeting).
    assert_eq!(
        unidentified_default_logs_for(&a_id),
        vec![crate::UNIDENTIFIED_DEFAULT_PLAN_LOG.to_owned()],
        "unidentified default must log exactly once for the state transition"
    );
    assert!(
        TRACING_LINES
            .lock()
            .unwrap()
            .iter()
            .any(|l| l.contains(crate::UNIDENTIFIED_DEFAULT_PLAN_LOG)),
        "tracing subscriber must observe the sender log line"
    );

    // Wait a bounded window first, then send the control message on the same mesh link and prove
    // the control arrives while the unidentified default and secondary reports were never delivered.
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    assert!(
        b.state.code.plan.accounts().is_empty(),
        "peer must receive nothing after bounded wait when sender default is unidentified: {:?}",
        b.state.code.plan.accounts()
    );

    emit(&a, &s_port.id, CodeEvent::PlanLimits { limits: report(0.77, 10) });
    until("B receives A's portable control report", 10, || {
        b.state.code.plan.account("u-port:o-port").latest.is_some()
    })
    .await;

    assert_eq!(
        b.state.code.plan.accounts().keys().collect::<Vec<_>>(),
        vec!["u-port:o-port"],
        "peer accounts() must contain ONLY the control account, never the unidentified default or secondary path: {:?}",
        b.state.code.plan.accounts()
    );
    let chans: crate::api::ChanFactory = Arc::new(|_| None);
    let views = crate::api::call(&b.state, "plan_accounts", json!({}), &chans).await.unwrap();
    let arr = views.as_array().unwrap();
    assert!(
        !arr.iter().any(|v| v["account"] == custom_path || v["account"] == default_path),
        "peer plan_accounts must never contain sender's path keys: {arr:?}"
    );

    // Now populate temp HOME's `.claude.json` with a portable default identity (`u-home:o-home`):
    // subsequent default-dir reports MUST be delivered to `b`, and a later transition back to
    // unidentified logs once again.
    std::fs::write(
        env.dir.join(".claude.json"),
        serde_json::json!({ "oauthAccount": {
            "accountUuid": "u-home", "organizationUuid": "o-home", "organizationName": "HomeOrg",
            "organizationRateLimitTier": "default_claude_ai"
        }})
        .to_string(),
    )
    .unwrap();
    emit(&a, &s_def.id, CodeEvent::PlanLimits { limits: report(0.62, 5) });
    until("B receives A's portable default report once identified", 10, || {
        b.state.code.plan.account("u-home:o-home").latest.is_some()
    })
    .await;
    assert_eq!(
        b.state.code.plan.account("u-home:o-home").latest.unwrap().five_hour.unwrap().utilization,
        0.62
    );

    // Transition back to unidentified (credentials tier mismatches profile) => logs a second time.
    std::fs::write(
        env.dir.join(".claude").join(".credentials.json"),
        r#"{"claudeAiOauth":{"accessToken":"t","refreshToken":"r","expiresAt":1,"rateLimitTier":"other_tier"}}"#,
    )
    .unwrap();
    emit(&a, &s_def.id, CodeEvent::PlanLimits { limits: report(0.65, 1) });
    assert_eq!(
        unidentified_default_logs_for(&a_id).len(),
        2,
        "transitioning from portable back to unidentified must log once for the new state change"
    );
}

/// Canonical default check (test 1 — default-misread-as-secondary prevention):
/// Symlinks to `~/.claude`, trailing slashes, and `CLAUDE_CONFIG_DIR` explicitly pointing at the
/// default directory (with trailing slash or symlink) are recognized as the default directory,
/// read `~/.claude.json` beside `.claude`, and never split into a secondary bucket.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn default_dir_via_symlink_trailing_slash_or_env_is_not_misread_as_secondary() {
    use code::acp::CodeEvent;
    use code::plan_limits::{PlanLimits, Window, now};
    let mut env = TempClaudeEnv::new(None, None).await;
    let real_default = env.dir.join(".claude");
    let link_default = env.dir.join("symlink-to-default-claude");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&real_default, &link_default).unwrap();
    #[cfg(not(unix))]
    std::fs::create_dir_all(&link_default).unwrap();

    let trailing = format!("{}/", real_default.to_string_lossy());
    let link_trailing = format!("{}/", link_default.to_string_lossy());

    // Explicitly set CLAUDE_CONFIG_DIR to the default location with a trailing slash.
    claude_auth::set_test_env(Some(env.dir.clone()), Some(trailing.clone()));
    assert!(crate::is_default_claude_dir_path(&real_default.to_string_lossy()));
    assert!(crate::is_default_claude_dir_path(&trailing));
    assert!(crate::is_default_claude_dir_path("~/.claude/"));
    #[cfg(unix)]
    assert!(crate::is_default_claude_dir_path(&link_trailing));

    // Before `.claude.json` exists: a session pointing at the symlink or trailing-slash default
    // normalizes onto the single default bucket (not a second secondary bucket) and logs once.
    let (a, b) = (node("alpha-canon-def"), node("bravo-canon-def"));
    link(&a, &b).await;
    let cwd = a.dir.to_string_lossy().into_owned();
    let s = a.state.db.create_code_session(&cwd, None, "claude-code", "default").unwrap();
    #[cfg(unix)]
    a.state
        .db
        .set_setting(&code::import_claude::config_dir_key(&s.id), &link_trailing)
        .unwrap();

    let report = |five: f64, ago: i64| PlanLimits {
        five_hour: Some(Window { utilization: five, resets_at: now() + 3600 }),
        observed_at: now() - ago,
        ..Default::default()
    };
    emit(&a, &s.id, CodeEvent::PlanLimits { limits: report(0.28, 30) });
    assert_eq!(
        a.state.code.plan.accounts().len(),
        1,
        "symlink/trailing-slash default must not create a second bucket: {:?}",
        a.state.code.plan.accounts()
    );
    assert_eq!(
        unidentified_default_logs_for(a.state.mesh.node_id()).len(),
        1,
        "unidentified default accessed via symlink/trailing-slash must still emit the visibility log"
    );

    // Once `~/.claude.json` (beside `.claude`, NOT inside `.claude/`) is written, accessing the
    // default dir via symlink or `CLAUDE_CONFIG_DIR` with trailing slash finds `u-canon:o-canon`
    // and broadcasts to `b`.
    std::fs::write(
        env.dir.join(".claude.json"),
        serde_json::json!({ "oauthAccount": {
            "accountUuid": "u-canon", "organizationUuid": "o-canon", "organizationName": "CanonOrg"
        }})
        .to_string(),
    )
    .unwrap();
    #[cfg(unix)]
    assert_eq!(
        claude_auth::account(&link_default),
        ("u-canon:o-canon".to_owned(), Some("CanonOrg".to_owned()))
    );
    emit(&a, &s.id, CodeEvent::PlanLimits { limits: report(0.54, 5) });
    until("B receives canonical default report", 10, || {
        b.state.code.plan.account("u-canon:o-canon").latest.is_some()
    })
    .await;
    assert_eq!(
        b.state.code.plan.account("u-canon:o-canon").latest.unwrap().five_hour.unwrap().utilization,
        0.54
    );
    let _ = &mut env;
}

/// Canonical default check (test 2 — secondary-misread-as-default prevention):
/// A secondary config directory named `.claude` (e.g. `<dir>/work/.claude/` or a symlink to it)
/// is NEVER misread as the default directory: its usage never reads `~/.claude.json` and never
/// folds into local or peers' default usage bucket as wrong data.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn secondary_dir_named_dot_claude_is_never_misread_as_default_or_folded_into_peers_default() {
    use code::acp::CodeEvent;
    use code::plan_limits::{PlanLimits, Window, now};
    let _env = TempClaudeEnv::new(Some(("u-main", "o-main", "MainOrg")), None).await;
    let (a, b) = (node("alpha-sec-dot-claude"), node("bravo-sec-dot-claude"));
    link(&a, &b).await;

    let cwd = a.dir.to_string_lossy().into_owned();
    let s_def = a.state.db.create_code_session(&cwd, None, "claude-code", "default").unwrap();

    let sec_dir = a.dir.join("work").join(".claude");
    std::fs::create_dir_all(&sec_dir).unwrap();
    let sec_link = a.dir.join("work-claude-link");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&sec_dir, &sec_link).unwrap();
    #[cfg(not(unix))]
    std::fs::create_dir_all(&sec_link).unwrap();

    let sec_with_slash = format!("{}/", sec_dir.to_string_lossy());
    assert!(!crate::is_default_claude_dir_path(&sec_with_slash));
    #[cfg(unix)]
    assert!(!crate::is_default_claude_dir_path(&sec_link.to_string_lossy()));

    let s_sec = a.state.db.create_code_session(&cwd, None, "claude-code", "default").unwrap();
    a.state
        .db
        .set_setting(&code::import_claude::config_dir_key(&s_sec.id), &sec_with_slash)
        .unwrap();

    let report = |five: f64, ago: i64| PlanLimits {
        five_hour: Some(Window { utilization: five, resets_at: now() + 3600 }),
        observed_at: now() - ago,
        ..Default::default()
    };

    // Seed the shared default account (`u-main:o-main`) at 0.10 on both peers.
    emit(&a, &s_def.id, CodeEvent::PlanLimits { limits: report(0.10, 60) });
    until("B has A's 0.10 default report", 10, || {
        b.state.code.plan.account("u-main:o-main").latest.is_some()
    })
    .await;

    // Emit 0.95 on the secondary `work/.claude/` session (which has no `.claude.json` yet), then
    // follow with a 0.12 control report on `s_def` to prove delivery on the mesh link.
    emit(&a, &s_sec.id, CodeEvent::PlanLimits { limits: report(0.95, 30) });
    emit(&a, &s_def.id, CodeEvent::PlanLimits { limits: report(0.12, 20) });
    until("B receives A's 0.12 default control update", 10, || {
        b.state
            .code
            .plan
            .account("u-main:o-main")
            .latest
            .and_then(|l| l.five_hour)
            .map(|w| w.utilization)
            == Some(0.12)
    })
    .await;

    // Secondary `0.95` report must NEVER fold into `u-main:o-main` on `a` or `b`, and must not
    // trigger the unidentified-default log line.
    assert_eq!(
        a.state.code.plan.account("u-main:o-main").latest.unwrap().five_hour.unwrap().utilization,
        0.12
    );
    assert_eq!(
        b.state.code.plan.account("u-main:o-main").latest.unwrap().five_hour.unwrap().utilization,
        0.12,
        "secondary work/.claude usage must never fold into peers' default bucket"
    );
    assert_eq!(
        b.state.code.plan.accounts().keys().collect::<Vec<_>>(),
        vec!["u-main:o-main"]
    );
    assert!(unidentified_default_logs_for(a.state.mesh.node_id()).is_empty());

    // When `work/.claude/.claude.json` is written with `u-sec:o-sec`, it propagates under its own
    // key and still leaves `u-main:o-main` untouched at 0.12.
    std::fs::write(
        sec_dir.join(".claude.json"),
        serde_json::json!({ "oauthAccount": {
            "accountUuid": "u-sec", "organizationUuid": "o-sec", "organizationName": "SecondOrg"
        }})
        .to_string(),
    )
    .unwrap();
    emit(&a, &s_sec.id, CodeEvent::PlanLimits { limits: report(0.88, 5) });
    until("B receives secondary account under u-sec:o-sec", 10, || {
        b.state.code.plan.account("u-sec:o-sec").latest.is_some()
    })
    .await;
    assert_eq!(
        b.state.code.plan.account("u-main:o-main").latest.unwrap().five_hour.unwrap().utilization,
        0.12
    );
    assert_eq!(
        b.state.code.plan.account("u-sec:o-sec").latest.unwrap().five_hour.unwrap().utilization,
        0.88
    );
}

/// Cursor plan usage one member fetched reaches the others; a different account is ignored.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cursor_plan_limits_reach_peers_on_the_same_account() {
    use code::cursor_plan::CursorPlanLimits;
    let (a, b) = (node("alpha-cursor-plan"), node("bravo-cursor-plan"));
    link(&a, &b).await;
    let report = |auto: f64, ago: i64, account: &str| CursorPlanLimits {
        account_id: account.into(),
        email: Some("a@b.c".into()),
        observed_at: code::cursor_plan::now() - ago,
        resets_at: Some(code::cursor_plan::now() + 86400),
        auto: Some(auto),
        api: Some(10.0),
        total: Some(auto),
        included_spend_cents: Some(20000),
        limit_cents: Some(40000),
        bonus_spend_cents: Some(500),
        remaining_bonus: Some(true),
        display_message: None,
        grants: Vec::new(),
        grants_remaining_cents: Some(0),
        slow_pool: Some(false),
        on_demand_allowed: Some(false),
    };
    adopt_cursor_plan(&a.state, report(40.0, 60, "42"), true);
    until("B has A's Cursor report", 10, || {
        b.state.code.cursor_plan.latest().is_some_and(|l| l.account_id == "42" && l.auto == Some(40.0))
    })
    .await;
    until("B's panel hears of it", 10, || {
        b.events
            .lock()
            .unwrap()
            .iter()
            .any(|(n, p)| n == "code://cursor-plan-limits" && p["auto"] == 40.0)
    })
    .await;

    // A newer report for the same account replaces; another account does not.
    adopt_cursor_plan(&a.state, report(55.0, 10, "42"), true);
    until("B takes the newer report", 10, || b.state.code.cursor_plan.latest().is_some_and(|l| l.auto == Some(55.0))).await;
    mesh::RpcHandler::event(
        &MeshRpc { state: b.state.clone() },
        MESH_CURSOR_PLAN_EVENT,
        &json!({ "accountId": "other", "limits": report(99.0, 5, "other") }),
    );
    assert_eq!(b.state.code.cursor_plan.latest().unwrap().auto, Some(55.0));
    assert_eq!(b.state.code.cursor_plan.latest().unwrap().account_id, "42");
}

/// A phone can decide a join request that waits on any computer, with its own signature.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn phone_decides_join_requests_waiting_on_any_computer() {
    let (a, b, c, d) = (node("alpha8"), node("bravo8"), node("charlie8"), node("delta8"));
    link(&a, &b).await;
    let p = phone();
    let pid = p.client.node_id();
    let url_a = format!("127.0.0.1:{}", a.state.mesh.bound_port().unwrap());
    p.client.join(&url_a, PASS, "pocket").unwrap();
    until("phone's join request", 10, || {
        a.state.mesh.status()["pending"].as_array().unwrap().iter().any(|r| r["nodeId"] == pid)
    })
    .await;
    a.state.mesh.approve(&pid).await.unwrap();
    until("phone online", 10, || p.client.status()["phase"] == "online").await;
    until("B knows the phone", 10, || members::get(&b.state.db, &pid).unwrap().is_some()).await;

    // C asks B; the phone, whichever of them it is connected through, is told and approves.
    until("B listens", 5, || b.state.mesh.bound_port().is_some()).await;
    let url_b = format!("127.0.0.1:{}", b.state.mesh.bound_port().unwrap());
    c.state.mesh.join(&url_b, PASS, "c").unwrap();
    let cid = c.state.mesh.node_id().to_owned();
    let waiting = |id: &str| p.client.status()["pending"].as_array().unwrap().iter().any(|r| r["nodeId"] == id);
    until("request reaches the phone", 10, || waiting(&cid)).await;
    until("joiner code", 5, || c.state.mesh.status()["joining"]["code"].is_string()).await;
    let seen = p.client.status()["pending"].as_array().unwrap().iter().find(|r| r["nodeId"] == cid.as_str()).cloned().unwrap();
    assert_eq!(seen["code"], c.state.mesh.status()["joining"]["code"], "the phone shows the joiner's code");
    assert_eq!(seen["host"], b.state.mesh.node_id());
    p.client.approve(&cid).await.unwrap();
    until("charlie joins", 10, || c.state.mesh.is_member()).await;
    assert_eq!(members::get(&c.state.db, &cid).unwrap().unwrap().approver, pid, "the admission carries the phone's signature");
    until("request clears", 10, || !waiting(&cid)).await;

    // And it can turn one away.
    d.state.mesh.join(&url_a, PASS, "d").unwrap();
    let did = d.state.mesh.node_id().to_owned();
    until("second request reaches the phone", 10, || waiting(&did)).await;
    p.client.reject(&did).await.unwrap();
    until("delta is turned away", 10, || d.state.mesh.status()["joining"]["status"] == "rejected").await;
    assert!(!d.state.mesh.is_member());
}

/// Next prompt for a stand-in agent: answers it as delivered and returns its message.
async fn take_agent_message(rx: &mut tokio::sync::mpsc::UnboundedReceiver<code::acp::AgentCmd>) -> code::acp::UserMessage {
    match tokio::time::timeout(Duration::from_secs(10), rx.recv()).await.expect("message arrives").expect("agent open") {
        code::acp::AgentCmd::Prompt { msg, force: false, reply } => {
            reply.unwrap().send(Ok(code::acp::Delivery::Sent)).unwrap();
            msg
        }
        _ => panic!("expected an agent message"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agents_message_each_other_across_the_mesh() {
    let (a, b) = (node("alpha-msg"), node("bravo-msg"));
    link(&a, &b).await;
    let (an, bn) = (a.state.mesh.node_id().to_owned(), b.state.mesh.node_id().to_owned());
    let (pa, pb) = (a.dir.join("proj"), b.dir.join("proj"));
    repo(&pa);
    repo(&pb);
    let sa = a.state.db.create_code_session(&pa.to_string_lossy(), None, "grok", "default").unwrap();
    let sb = b.state.db.create_code_session(&pb.to_string_lossy(), None, "grok", "default").unwrap();
    until("sessions replicate", 10, || {
        a.state.db.get_code_session(&sb.id).unwrap().is_some() && b.state.db.get_code_session(&sa.id).unwrap().is_some()
    })
    .await;
    let mut rx_a = a.state.code.attach_test_agent(&sa.id);
    let mut rx_b = b.state.code.attach_test_agent(&sb.id);

    // A -> B, through the mesh to B's host.
    let (r, got) = tokio::join!(send_agent_message(&a.state, &an, &sa.id, &sb.id, "ping"), take_agent_message(&mut rx_b));
    assert_eq!(r.unwrap()["delivery"], "sent");
    assert!(got.text.contains("ping") && got.text.contains(&sa.id), "{}", got.text);
    assert_eq!(got.origin, format!("agent:{an}:{}", sa.id));

    // B -> A: the reply path.
    let (r, got) = tokio::join!(send_agent_message(&b.state, &bn, &sb.id, &sa.id, "pong"), take_agent_message(&mut rx_a));
    r.unwrap();
    assert!(got.text.contains("pong") && got.text.contains(&sb.id), "{}", got.text);
    assert!(
        got.text.contains("Do not resend just because a prior delivery was queued"),
        "anti-loop footer present: {}",
        got.text
    );

    // An identical retry is accepted without a second prompt.
    let again = send_agent_message(&b.state, &bn, &sb.id, &sa.id, "pong").await.unwrap();
    assert_eq!(again["delivery"], "duplicate");
    assert_eq!(again["accepted"], true);
    assert!(
        tokio::time::timeout(Duration::from_millis(200), rx_a.recv()).await.is_err(),
        "duplicate must not re-prompt the target"
    );

    // Guards: no self-messages, and a node can't send as a session it doesn't host.
    assert!(send_agent_message(&a.state, &an, &sa.id, &sa.id, "x").await.is_err());
    assert!(send_agent_message(&a.state, &an, &sb.id, &sa.id, "x").await.is_err());
}

/// An agent creates a sibling session: it starts in the folder with the creator's approval mode,
/// gets the brief, shows in the list, and the limits hold.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_agent_creates_a_session_within_its_limits() {
    let a = node("alpha-create");
    let an = a.state.mesh.node_id().to_owned();
    let proj = a.dir.join("proj");
    let work = proj.join("work");
    repo(&proj);
    std::fs::create_dir_all(&work).unwrap();
    let sa = a.state.db.create_code_session(&proj.to_string_lossy(), None, "grok", "auto").unwrap();
    let _rx_a = a.state.code.attach_test_agent(&sa.id);
    let (started_tx, mut started) = tokio::sync::mpsc::unbounded_channel();
    *a.state.code.test_started.lock().unwrap() = Some(started_tx);

    let cwd = work.to_string_lossy().into_owned();
    let create = create_agent_session(&a.state, &sa.id, &cwd, "Build the thing", Some("Builder"), None, None);
    let child_side = async {
        let (id, mut rx) = started.recv().await.expect("the agent is started");
        (id, take_agent_message(&mut rx).await)
    };
    let (r, (child_id, got)) = tokio::join!(create, child_side);
    let r = r.unwrap();
    assert_eq!(r["sessionId"], child_id.as_str());
    assert!(got.text.starts_with("Build the thing") && got.text.contains(&sa.id), "{}", got.text);
    assert_eq!(got.origin, format!("agent:{an}:{}", sa.id));

    let child = a.state.db.get_code_session(&child_id).unwrap().unwrap();
    assert_eq!(child.permission_mode, "auto", "inherits the creator's mode");
    assert_eq!(child.created_by.as_deref(), Some(sa.id.as_str()));
    assert_eq!((child.title.as_str(), child.agent.as_str(), child.cwd.as_str()), ("Builder", "grok", std::fs::canonicalize(&work).unwrap().to_str().unwrap()));
    let listed = list_agent_sessions(&a.state, &sa.id).unwrap();
    assert!(listed.as_array().unwrap().iter().any(|s| s["sessionId"] == child_id.as_str()));

    // Guards: a folder that doesn't exist, an empty brief, and an unknown agent.
    let missing = a.dir.join("nope").to_string_lossy().into_owned();
    assert!(create_agent_session(&a.state, &sa.id, &missing, "x", None, None, None).await.is_err());
    assert!(create_agent_session(&a.state, &sa.id, &cwd, "  ", None, None, None).await.is_err());
    assert!(create_agent_session(&a.state, &sa.id, &cwd, "x", None, Some("nope"), None).await.is_err());

    // Depth: a session created by an agent may create (depth 2), but its own child may not.
    let grand = a.state.db.create_code_session(&cwd, None, "grok", "auto").unwrap();
    a.state.db.set_code_session_created_by(&grand.id, &child_id).unwrap();
    let err = create_agent_session(&a.state, &grand.id, &cwd, "x", None, None, None).await.unwrap_err();
    assert!(err.to_string().contains("depth"), "{err}");

    // Cap: five created sessions, then no more (the one above counts).
    for _ in 0..MAX_CREATED_SESSIONS - 1 {
        let s = a.state.db.create_code_session(&cwd, None, "grok", "auto").unwrap();
        a.state.db.set_code_session_created_by(&s.id, &sa.id).unwrap();
    }
    let err = create_agent_session(&a.state, &sa.id, &cwd, "x", None, None, None).await.unwrap_err();
    assert!(err.to_string().contains("at most"), "{err}");
}

/// A restrictive creator can't spawn a looser session, whatever the global preference or agent.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_agent_created_session_is_never_looser_than_its_creator() {
    let a = node("alpha-create-mode");
    let proj = a.dir.join("proj");
    let work = proj.join("work");
    repo(&proj);
    std::fs::create_dir_all(&work).unwrap();
    let mut prefs = prefs::load(&a.state.db);
    prefs.permission_mode = "always-approve".into();
    prefs::save(&a.state.db, &prefs).unwrap();
    let sa = a.state.db.create_code_session(&proj.to_string_lossy(), None, "grok", "default").unwrap();
    let _rx_a = a.state.code.attach_test_agent(&sa.id);
    let (started_tx, mut started) = tokio::sync::mpsc::unbounded_channel();
    *a.state.code.test_started.lock().unwrap() = Some(started_tx);

    let cwd = work.to_string_lossy().into_owned();
    let create = create_agent_session(&a.state, &sa.id, &cwd, "x", None, Some("opencode"), None);
    let child_side = async {
        let (_, mut rx) = started.recv().await.expect("the agent is started");
        take_agent_message(&mut rx).await
    };
    let (r, _) = tokio::join!(create, child_side);
    assert_eq!(r.unwrap()["permissionMode"], "default");
}

/// A session whose agent fails to start is removed, so it doesn't use up the creator's cap.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_agent_creation_leaves_no_session() {
    let a = node("alpha-create-fail");
    let proj = a.dir.join("proj");
    let work = proj.join("work");
    repo(&proj);
    std::fs::create_dir_all(&work).unwrap();
    let sa = a.state.db.create_code_session(&proj.to_string_lossy(), None, "grok", "default").unwrap();
    let (started_tx, started) = tokio::sync::mpsc::unbounded_channel();
    drop(started);
    *a.state.code.test_started.lock().unwrap() = Some(started_tx);

    let before = a.state.db.list_code_sessions().unwrap().len();
    let cwd = work.to_string_lossy().into_owned();
    assert!(create_agent_session(&a.state, &sa.id, &cwd, "x", None, None, None).await.is_err());
    assert_eq!(a.state.db.list_code_sessions().unwrap().len(), before);
    assert_eq!(a.state.db.count_created_by(&sa.id).unwrap(), 0);
}

/// The MCP tool call reaches `create_agent_session` through the loopback bridge with its arguments.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_mcp_create_tool_reaches_the_app_through_the_bridge() {
    let a = node("alpha-create-bridge");
    let proj = a.dir.join("proj");
    let work = proj.join("work");
    repo(&proj);
    std::fs::create_dir_all(&work).unwrap();
    let sa = a.state.db.create_code_session(&proj.to_string_lossy(), None, "grok", "auto").unwrap();
    let _rx_a = a.state.code.attach_test_agent(&sa.id);
    let (started_tx, mut started) = tokio::sync::mpsc::unbounded_channel();
    *a.state.code.test_started.lock().unwrap() = Some(started_tx);

    let (bridge, listener) = agent_messaging::AgentMessagingBridge::bind(None).unwrap();
    let def = bridge.server_definition(&sa.id, "grok", true).unwrap();
    bridge.serve(listener, a.state.clone());
    let env = |k: &str| def["env"].as_array().unwrap().iter().find(|e| e["name"] == k).unwrap()["value"].as_str().unwrap().to_owned();

    let params = json!({ "name": "create_agent_session", "arguments": { "cwd": work.to_string_lossy(), "prompt": "via bridge", "title": "Bridged" } });
    let (addr, token) = (env("PATCHBAY_MESSAGE_ADDR"), env("PATCHBAY_MESSAGE_TOKEN"));
    let call = agent_messaging::call_tool(&addr, &token, &sa.id, &params);
    let child_side = async {
        let (id, mut rx) = started.recv().await.expect("the agent is started");
        (id, take_agent_message(&mut rx).await)
    };
    let (r, (id, got)) = tokio::join!(call, child_side);
    let r = r.unwrap();
    assert_eq!(r["isError"], false, "{r}");
    assert!(got.text.starts_with("via bridge"), "{}", got.text);
    let child = a.state.db.get_code_session(&id).unwrap().unwrap();
    assert_eq!((child.title.as_str(), child.created_by.as_deref()), ("Bridged", Some(sa.id.as_str())));
}

/// A child can't reach beyond its creator's folder, and an agent that acts without asking can't be
/// chosen from a session that isn't in always-approve mode.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_agent_created_session_stays_in_its_creators_folder_and_approvals() {
    let a = node("alpha-create-scope");
    let proj = a.dir.join("proj");
    let work = proj.join("work");
    repo(&proj);
    std::fs::create_dir_all(&work).unwrap();
    let sa = a.state.db.create_code_session(&proj.to_string_lossy(), None, "grok", "default").unwrap();
    let outside = a.dir.to_string_lossy().into_owned();
    let sneaky = proj.join("work").join("..").join("..").to_string_lossy().into_owned();
    for cwd in [outside.as_str(), sneaky.as_str(), ".", "/"] {
        let err = create_agent_session(&a.state, &sa.id, cwd, "x", None, None, None).await.unwrap_err();
        assert!(err.to_string().contains("cwd must be") || err.to_string().contains("not a usable"), "{cwd}: {err}");
    }
    let cwd = work.to_string_lossy().into_owned();
    for gapped in [code::agents::AgentKind::Cursor.id(), code::agents::AgentKind::ClaudeCode.id()] {
        let err = create_agent_session(&a.state, &sa.id, &cwd, "x", None, Some(gapped), None).await.unwrap_err();
        assert!(err.to_string().contains("without asking"), "{gapped}: {err}");
    }
    assert_eq!(a.state.db.count_created_by(&sa.id).unwrap(), 0, "refusals create nothing");

    // From always-approve the same agent is allowed.
    a.state.db.set_code_session_permission_mode(&sa.id, "always-approve").unwrap();
    let _rx_a = a.state.code.attach_test_agent(&sa.id);
    let (started_tx, mut started) = tokio::sync::mpsc::unbounded_channel();
    *a.state.code.test_started.lock().unwrap() = Some(started_tx);
    let create = create_agent_session(&a.state, &sa.id, &cwd, "x", None, Some(code::agents::AgentKind::Cursor.id()), None);
    let child_side = async {
        let (_, mut rx) = started.recv().await.expect("the agent is started");
        take_agent_message(&mut rx).await
    };
    let (r, _) = tokio::join!(create, child_side);
    assert_eq!(r.unwrap()["agent"], code::agents::AgentKind::Cursor.id());
}

/// Parallel creates can't pass the cap together.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_creates_respect_the_cap() {
    let a = node("alpha-create-race");
    let proj = a.dir.join("proj");
    repo(&proj);
    let cwd = proj.to_string_lossy().into_owned();
    let sa = a.state.db.create_code_session(&cwd, None, "grok", "default").unwrap();
    let _rx_a = a.state.code.attach_test_agent(&sa.id);
    let (started_tx, mut started) = tokio::sync::mpsc::unbounded_channel();
    *a.state.code.test_started.lock().unwrap() = Some(started_tx);
    tokio::spawn(async move {
        while let Some((_, mut rx)) = started.recv().await {
            tokio::spawn(async move {
                take_agent_message(&mut rx).await;
            });
        }
    });

    let mut set = tokio::task::JoinSet::new();
    for _ in 0..MAX_CREATED_SESSIONS + 3 {
        let (state, id, cwd) = (a.state.clone(), sa.id.clone(), cwd.clone());
        set.spawn(async move { create_agent_session(&state, &id, &cwd, "x", None, None, None).await.is_ok() });
    }
    let mut made = 0;
    while let Some(ok) = set.join_next().await {
        made += ok.unwrap() as i64;
    }
    assert_eq!(made, MAX_CREATED_SESSIONS);
    assert_eq!(a.state.db.count_created_by(&sa.id).unwrap(), MAX_CREATED_SESSIONS);
}

/// A window on B that missed some of A's events for a session catches up through `code_resume`:
/// the events after its last point while A's ring has them, else a snapshot with the status of
/// the tool calls it still shows running.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn missed_session_events_resume_from_the_host() {
    let (a, b) = (node("alpha-resume"), node("bravo-resume"));
    link(&a, &b).await;
    let proj = a.dir.join("proj");
    repo(&proj);
    let s = a.state.db.create_code_session(&proj.to_string_lossy(), None, "grok", "default").unwrap();
    until("session reaches B", 10, || b.state.db.get_code_session(&s.id).unwrap().is_some()).await;

    let update = |u: Value| code::acp::Envelope { code_session_id: s.id.clone(), event: code::acp::CodeEvent::Update { update: u } };
    on_code_event(&a.state, update(json!({ "sessionUpdate": "tool_call", "toolCallId": "t1", "title": "Watch CI" })));
    let first = a.state.mesh.code_point(&s.id).expect("a point for the session");
    on_code_event(&a.state, update(json!({ "sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "in_progress" })));
    on_code_event(&a.state, update(json!({ "sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "completed" })));

    // From the first point: the two later updates, folded into one.
    let point = serde_json::to_value(&first).unwrap();
    let r = code_resume(&b.state, s.id.clone(), Some(point), Some(vec!["t1".into()])).await.unwrap();
    assert_eq!(r["mode"], "delta", "{r}");
    let events = r["events"].as_array().unwrap();
    assert_eq!(events.len(), 1, "{r}");
    assert_eq!(events[0]["event"]["update"]["status"], "completed");
    assert_eq!(r["point"]["n"], first.n + 2);

    // Without a point A can resume from: a snapshot, with the call's status on A.
    let r = code_resume(&b.state, s.id.clone(), None, Some(vec!["t1".into(), "gone".into()])).await.unwrap();
    assert_eq!(r["mode"], "snapshot", "{r}");
    assert_eq!(r["tools"], json!({ "t1": "completed" }));
    assert_eq!(r["attach"]["busy"], false);
}

/// A window on B opening a session A hosts gets the agent's run so far when no transcript is
/// saved, and its save of the transcript goes to A, the only writer of the cache.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remote_window_catches_up_and_saves_through_the_host() {
    let (a, b) = (node("alpha-cache"), node("bravo-cache"));
    link(&a, &b).await;
    let proj = a.dir.join("proj");
    repo(&proj);
    let s = a.state.db.create_code_session(&proj.to_string_lossy(), None, "grok", "default").unwrap();
    until("session reaches B", 10, || b.state.db.get_code_session(&s.id).unwrap().is_some()).await;

    let ev = |e: code::acp::CodeEvent| on_code_event(&a.state, code::acp::Envelope { code_session_id: s.id.clone(), event: e });
    ev(code::acp::CodeEvent::Status { state: "starting".into(), message: None });
    ev(code::acp::CodeEvent::Update { update: json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "hello" } }) });

    // No transcript saved: the whole run, held whole by A's ring.
    let attach = code_attach(&b.state, s.id.clone(), None, None).await.unwrap();
    assert_eq!(attach["partial"], false, "{attach}");
    let events = attach["events"].as_array().unwrap();
    assert!(events.iter().any(|e| e["update"]["content"]["text"] == "hello"), "{attach}");
    assert!(attach["point"]["n"].as_u64().is_some(), "{attach}");

    // B's save lands on A.
    save_code_transcript(&b.state, s.id.clone(), "{\"v\":1}".into()).await.unwrap();
    assert_eq!(a.state.db.code_transcript(&s.id).unwrap().as_deref(), Some("{\"v\":1}"));
}
