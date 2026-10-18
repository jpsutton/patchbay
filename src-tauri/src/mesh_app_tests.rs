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

    // Removed on B: the phone is told and forgets the mesh.
    b.state.mesh.remove(&pid).unwrap();
    until("phone forgets the mesh", 10, || !p.client.is_member()).await;
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
