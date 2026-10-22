//! Two app instances in one process, linked by a real mesh on localhost: moving a Code session,
//! checkpoints, taking over from an offline host, and remote terminals.

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
    let dir = std::env::temp_dir().join(format!("pb-phone-{}", db::new_id()));
    let events = Arc::new(Mutex::new(Vec::new()));
    let ev = events.clone();
    let sink: patchbay_mesh::client::Sink = Arc::new(move |n: &str, p: Value| ev.lock().unwrap().push((n.to_owned(), p)));
    let client = patchbay_mesh::client::RemoteClient::new(dir, Arc::new(MemKeyStore::default()), sink).unwrap();
    client.start();
    Phone { client, events }
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
    a.state.mesh.approve(&pid).unwrap();
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
    b.state.mesh.approve(&pid).unwrap();
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
    a.state.mesh.approve(&pid).unwrap();
    until("phone online", 10, || p.client.status()["phase"] == "online").await;
    let bn = b.state.mesh.node_id().to_owned();

    // What B offers, asked through A.
    let opts = p.client.rpc("remote.new_session_options", json!({ "host": bn })).await.unwrap();
    assert!(opts["agents"].as_array().unwrap().iter().any(|x| x["id"] == "grok"));
    assert_eq!(opts["defaultAgent"], "grok");

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
    a.state.mesh.approve(&pid).unwrap();
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
/// `scripts/mock-llm.py` (start it first) and a Grok Build session using it. Every phone that
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
    let s = db.create_code_session(&proj.to_string_lossy(), Some("m-agent"), "grok", "default").unwrap();
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
            let _ = n.state.mesh.approve(id);
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

/// The usage guard: a Claude Code turn past the plan limit is cancelled once, with a notice, and
/// new prompts are refused until the window resets. Sessions on other agents run on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn usage_limit_stops_claude_turns() {
    use code::acp::{AgentCmd, CodeEvent, Delivery};
    use code::plan_limits::PlanLimits;
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
    assert!(text.starts_with("Stopped. Claude Code has used 96% of your plan's 5-hour limit"), "{text}");

    emit(&a, &claude.id, CodeEvent::PromptDone { stop_reason: json!("cancelled") });
    let err = prompt(claude.id.clone()).await.unwrap_err();
    assert!(err.0.starts_with("Not sent. Claude Code has used 97%"), "{}", err.0);

    // Turning the guard off lets prompts through again.
    let mut prefs = prefs::load(&a.state.db);
    prefs.usage_limit.enabled = false;
    crate::api::call(&a.state, "set_preferences", json!({ "prefs": prefs }), &chans).await.unwrap();
    assert!(a.state.code.plan.hit(&prefs::load(&a.state.db).usage_limit, code::plan_limits::now()).is_none());

    // Lowering the limit below current usage stops a running turn right away.
    emit(&a, &claude.id, CodeEvent::UserPrompt {
        prompt_id: db::new_id(),
        text: "go".into(),
        origin: String::new(),
        attachments: json!([]),
        delivery: Delivery::Sent,
    });
    prefs.usage_limit = prefs::UsageLimit { enabled: true, percent: 50 };
    crate::api::call(&a.state, "set_preferences", json!({ "prefs": prefs }), &chans).await.unwrap();
    assert!(matches!(claude_rx.try_recv(), Ok(AgentCmd::Cancel)));
    assert_eq!(notices(), 2);

    // The panel reads the last report, which a restart keeps.
    let v = crate::api::call(&a.state, "plan_limits", json!({}), &chans).await.unwrap();
    assert_eq!(v["fiveHour"]["utilization"], 0.97);
    assert!(a.events.lock().unwrap().iter().any(|(n, p)| n == "code://plan-limits" && p["sevenDay"]["utilization"] == 0.3));
    let again = AppState::new(a.state.db.clone(), a.dir.clone(), a.state.mesh.clone(), Arc::new(|_: &str, _: Value| {}));
    assert_eq!(again.code.plan.latest().and_then(|l| l.five_hour).map(|w| w.utilization), Some(0.97));
}
